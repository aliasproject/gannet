//! Finding a file's thumbnail in the freedesktop cache that other programs fill. Only
//! finds and checks: thumbnails are decoded elsewhere, like any other image.

use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// Biggest first, so a preview starts as sharp as it can.
const SIZES: [&str; 4] = ["xx-large", "x-large", "large", "normal"];

/// A thumbnail bigger than this isn't one anybody meant to write.
pub const MAX_THUMBNAIL: u64 = 32 * 1024 * 1024;

/// The URI a thumbnail's name is the hash of. The cache is shared, so this has to match what
/// GLib writes byte for byte, and GLib leaves more characters alone than `uri::to_file_uri`.
pub fn thumbnail_uri(path: &Path) -> String {
    let mut out = String::from("file://");
    for &b in path.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || b"!$&'()*+,-./:=@_~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Where a thumbnail of this file could be, in the order to try them.
pub fn candidates(cache_home: &Path, path: &Path) -> Vec<PathBuf> {
    let name = format!("{:x}.png", md5::compute(thumbnail_uri(path).as_bytes()));
    SIZES
        .iter()
        .map(|size| cache_home.join("thumbnails").join(size).join(&name))
        .collect()
}

/// The modified time a thumbnail says it was made from, if it says. A thumbnail that doesn't
/// match its file is out of date and shows the wrong picture.
pub fn recorded_mtime(png: &[u8]) -> Option<i64> {
    let mut rest = png.strip_prefix(b"\x89PNG\r\n\x1a\n")?;
    while rest.len() >= 8 {
        let len = u32::from_be_bytes(rest[..4].try_into().ok()?) as usize;
        let kind = &rest[4..8];
        let body = rest.get(8..8usize.checked_add(len)?)?;
        if kind == b"IEND" {
            return None;
        }
        if kind == b"tEXt"
            && let Some(value) = body.strip_prefix(b"Thumb::MTime\0")
        {
            return std::str::from_utf8(value).ok()?.trim().parse().ok();
        }
        rest = rest.get(12 + len..)?;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(kind: &[u8], body: &[u8]) -> Vec<u8> {
        let mut c = (body.len() as u32).to_be_bytes().to_vec();
        c.extend_from_slice(kind);
        c.extend_from_slice(body);
        c.extend_from_slice(&[0; 4]);
        c
    }

    fn png(chunks: &[Vec<u8>]) -> Vec<u8> {
        let mut p = b"\x89PNG\r\n\x1a\n".to_vec();
        for c in chunks {
            p.extend_from_slice(c);
        }
        p
    }

    #[test]
    fn uris_match_what_glib_writes() {
        assert_eq!(
            thumbnail_uri(Path::new("/a b/it's (1)+@,_-~.png")),
            "file:///a%20b/it's%20(1)+@,_-~.png"
        );
        assert_eq!(thumbnail_uri(Path::new("/x#y%z?")), "file:///x%23y%25z%3F");
        assert_eq!(thumbnail_uri(Path::new("/é")), "file:///%C3%A9");
    }

    #[test]
    fn the_name_is_the_hash_of_the_uri() {
        // From the freedesktop spec's example, checked with md5sum.
        let found = candidates(Path::new("/c"), Path::new("/home/jens/photos/me.png"));
        assert_eq!(
            found[3],
            Path::new("/c/thumbnails/normal/c6ee772d9e49320e97ec29a7eb5b1697.png")
        );
        assert!(found[0].starts_with("/c/thumbnails/xx-large"));
    }

    #[test]
    fn reads_the_recorded_time() {
        let p = png(&[
            chunk(b"IHDR", &[0; 13]),
            chunk(b"tEXt", b"Software\0x"),
            chunk(b"tEXt", b"Thumb::MTime\x001234"),
            chunk(b"IEND", b""),
        ]);
        assert_eq!(recorded_mtime(&p), Some(1234));
    }

    #[test]
    fn bad_or_missing_times_are_none() {
        assert_eq!(recorded_mtime(b"not a png"), None);
        assert_eq!(recorded_mtime(&png(&[chunk(b"IEND", b"")])), None);
        assert_eq!(
            recorded_mtime(&png(&[chunk(b"tEXt", b"Thumb::MTime\0soon")])),
            None
        );
        // A length that runs off the end, and one big enough to overflow.
        let mut cut = png(&[chunk(b"tEXt", b"Thumb::MTime\x001234")]);
        cut.truncate(cut.len() - 8);
        assert_eq!(recorded_mtime(&cut), None);
        let mut huge = b"\x89PNG\r\n\x1a\n".to_vec();
        huge.extend_from_slice(&[0xff; 4]);
        huge.extend_from_slice(b"tEXt");
        assert_eq!(recorded_mtime(&huge), None);
    }
}
