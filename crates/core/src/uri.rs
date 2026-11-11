//! Percent-encoding for `file://` URIs and `.trashinfo` paths. Works on bytes, so
//! names that aren't UTF-8 survive.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// Encodes a path, leaving `/` and unreserved characters as they are.
pub fn encode_path(path: &Path) -> String {
    let mut out = String::with_capacity(path.as_os_str().len());
    for &b in path.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'/' | b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(hex(b >> 4));
            out.push(hex(b & 15));
        }
    }
    out
}

fn hex(n: u8) -> char {
    char::from_digit(n as u32, 16).unwrap().to_ascii_uppercase()
}

/// Decodes percent-escapes. A malformed escape stays as literal text.
pub fn decode_path(text: &str) -> PathBuf {
    let b = text.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hi = (b[i + 1] as char).to_digit(16);
            let lo = (b[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    PathBuf::from(OsStr::from_bytes(&out))
}

pub fn to_file_uri(path: &Path) -> String {
    format!("file://{}", encode_path(path))
}

/// The local path in a `file:` URI. Other schemes and other hosts give `None`.
pub fn from_file_uri(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://").or_else(|| uri.strip_prefix("file:"))?;
    if let Some(path) = rest.strip_prefix('/') {
        return Some(decode_path(&format!("/{path}")));
    }
    let (host, path) = rest.split_once('/')?;
    (host == "localhost").then(|| decode_path(&format!("/{path}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_awkward_names() {
        for raw in [&b"/a b/c%d"[..], b"/caf\xc3\xa9/\xff\xfe", b"/#?&[]+"] {
            let p = PathBuf::from(OsStr::from_bytes(raw));
            assert_eq!(decode_path(&encode_path(&p)), p);
            assert_eq!(from_file_uri(&to_file_uri(&p)).unwrap(), p);
        }
    }

    #[test]
    fn encodes_like_the_spec_example() {
        assert_eq!(encode_path(Path::new("/foo/a b.txt")), "/foo/a%20b.txt");
    }

    #[test]
    fn rejects_other_schemes_and_hosts() {
        assert_eq!(from_file_uri("https://x/y"), None);
        assert_eq!(from_file_uri("file://otherhost/y"), None);
        assert_eq!(from_file_uri("file://localhost/y"), Some("/y".into()));
        assert_eq!(from_file_uri("file:/y"), Some("/y".into()));
    }

    #[test]
    fn stray_percents_stay_literal() {
        assert_eq!(decode_path("/a%zz%"), PathBuf::from("/a%zz%"));
        assert_eq!(decode_path("/a%4"), PathBuf::from("/a%4"));
    }
}
