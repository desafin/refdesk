//! Link URIs that MuPDF did not resolve to a page of the open document.
//!
//! MuPDF turns every link action into a URI. Destinations inside the document come back already
//! resolved; what is left is either an address for another program (`https:`, `mailto:` ...) or
//! a file (`GoToR` and `Launch` actions become `file:` URIs, see `pdf-link.c` in MuPDF).

use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};

use crate::link::LinkTarget;
use crate::nav::dest::{DestKind, Destination};

/// Classifies a link URI that has no resolved in-document destination.
///
/// Relative file paths are taken relative to `base_dir`, the directory of the open document.
/// Returns `None` for URIs that cannot be followed (empty, or an unresolved `#` fragment).
pub(super) fn classify(uri: &str, base_dir: Option<&Path>) -> Option<LinkTarget> {
    let uri = uri.trim();
    if uri.is_empty() || uri.starts_with('#') {
        return None;
    }
    let file_part = match strip_prefix_ignore_case(uri, "file:") {
        Some(rest) => {
            let rest = rest.strip_prefix("//").unwrap_or(rest);
            // `file://localhost/x` names the local file `/x`.
            strip_prefix_ignore_case(rest, "localhost/")
                .map(|_| &rest["localhost".len()..])
                .unwrap_or(rest)
        }
        None if has_scheme(uri) => return Some(LinkTarget::External(uri.to_string())),
        // No scheme at all: a bare relative file name.
        None => uri,
    };
    let (path_part, fragment) = match file_part.split_once('#') {
        Some((p, f)) => (p, Some(f)),
        None => (file_part, None),
    };
    if path_part.is_empty() {
        return None;
    }
    let path = PathBuf::from(OsString::from_vec(percent_decode(path_part)));
    let path = match base_dir {
        Some(dir) if path.is_relative() => dir.join(path),
        _ => path,
    };
    // Only the page of a remote destination is kept: its coordinates are in the other
    // document's page space, which is unknown until that document is opened.
    let dest = fragment.and_then(fragment_page).map(|page| Destination {
        page,
        kind: DestKind::Fit,
    });
    Some(LinkTarget::OtherDocument { path, dest })
}

fn strip_prefix_ignore_case<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let head = s.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &s[prefix.len()..])
}

/// Whether `uri` starts with an RFC 3986 scheme followed by `:` (as `fz_is_external_link`).
fn has_scheme(uri: &str) -> bool {
    let mut chars = uri.chars();
    if !chars.next().is_some_and(|c| c.is_ascii_alphabetic()) {
        return false;
    }
    for c in chars {
        if c == ':' {
            return true;
        }
        if !(c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.') {
            return false;
        }
    }
    false
}

/// Zero-based page of a `page=N` fragment parameter (N is 1-based).
fn fragment_page(fragment: &str) -> Option<u32> {
    fragment
        .split('&')
        .find_map(|kv| kv.strip_prefix("page="))
        .and_then(|n| n.parse::<u32>().ok())
        .and_then(|n| n.checked_sub(1))
}

/// Decodes `%XX` escapes. Malformed escapes are kept as written.
fn percent_decode(s: &str) -> Vec<u8> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let (Some(h), Some(l)) = (
                bytes.get(i + 1).and_then(|b| hex(*b)),
                bytes.get(i + 2).and_then(|b| hex(*b)),
            )
        {
            out.push(h * 16 + l);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

fn hex(b: u8) -> Option<u8> {
    (b as char).to_digit(16).map(|d| d as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn other(path: &str, page: Option<u32>) -> Option<LinkTarget> {
        Some(LinkTarget::OtherDocument {
            path: PathBuf::from(path),
            dest: page.map(|page| Destination {
                page,
                kind: DestKind::Fit,
            }),
        })
    }

    #[test]
    fn web_and_mail_addresses_are_external() {
        for uri in [
            "https://example.com/a?b#c",
            "http://x",
            "mailto:a@b.c",
            "ftp://h/f",
        ] {
            assert_eq!(
                classify(uri, None),
                Some(LinkTarget::External(uri.to_string()))
            );
        }
    }

    #[test]
    fn absolute_file_uri_with_page() {
        assert_eq!(
            classify(
                "file:///docs/other%20spec.pdf#page=3&zoom=nan,72,100",
                Some(Path::new("/here"))
            ),
            other("/docs/other spec.pdf", Some(2))
        );
        assert_eq!(
            classify("file://localhost/docs/a.pdf", None),
            other("/docs/a.pdf", None)
        );
    }

    #[test]
    fn relative_file_uri_uses_the_document_directory() {
        let base = Some(Path::new("/home/u/docs"));
        assert_eq!(
            classify("file:sub/b.pdf#nameddest=intro", base),
            other("/home/u/docs/sub/b.pdf", None)
        );
        assert_eq!(classify("c.pdf", base), other("/home/u/docs/c.pdf", None));
        // MuPDF prefixes a relative URI action with "file://".
        assert_eq!(
            classify("file://d.pdf#page=1", base),
            other("/home/u/docs/d.pdf", Some(0))
        );
    }

    #[test]
    fn unfollowable_uris() {
        assert_eq!(classify("", None), None);
        assert_eq!(classify("#page=4", None), None);
        assert_eq!(classify("file:#page=2", None), None);
    }

    #[test]
    fn percent_decoding_keeps_malformed_escapes() {
        assert_eq!(percent_decode("a%2Fb%zz%4"), b"a/b%zz%4".to_vec());
    }
}
