//! Finding a file's thumbnail in the freedesktop cache that other programs fill, and where Gannet
//! puts the ones it makes. Only finds and checks: thumbnails are decoded and written elsewhere.

use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// Biggest first, so a preview starts as sharp as it can.
const SIZES: [&str; 4] = ["xx-large", "x-large", "large", "normal"];
/// The size Gannet makes: at most this many pixels either way.
pub const NORMAL_PIXELS: u32 = 128;
/// Where Gannet notes the files it couldn't make a thumbnail of, inside `thumbnails/fail`.
const FAIL_FOLDER: &str = "gannet";

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

/// The file's name in each size folder, and in the failures folder.
fn file_name(path: &Path) -> String {
    format!("{:x}.png", md5::compute(thumbnail_uri(path).as_bytes()))
}

/// Where a thumbnail of this file could be, biggest first.
pub fn candidates(cache_home: &Path, path: &Path) -> Vec<PathBuf> {
    let name = file_name(path);
    SIZES.iter().map(|size| cache_home.join("thumbnails").join(size).join(&name)).collect()
}

/// Where the thumbnail Gannet makes of a file goes.
pub fn normal(cache_home: &Path, path: &Path) -> PathBuf {
    cache_home.join("thumbnails/normal").join(file_name(path))
}

/// Where Gannet notes that it couldn't make a thumbnail of a file, so it isn't tried again until
/// the file changes.
pub fn failure(cache_home: &Path, path: &Path) -> PathBuf {
    cache_home.join("thumbnails/fail").join(FAIL_FOLDER).join(file_name(path))
}

/// Whether a file is in the thumbnail cache itself. The spec says not to make thumbnails of
/// those: they're small pictures already, and each would make another.
pub fn is_inside(cache_home: &Path, path: &Path) -> bool {
    path.starts_with(cache_home.join("thumbnails"))
}

/// The value of one of a PNG's `tEXt` entries, if it has it.
pub fn text<'a>(png: &'a [u8], key: &str) -> Option<&'a [u8]> {
    let mut rest = png.strip_prefix(b"\x89PNG\r\n\x1a\n")?;
    while rest.len() >= 8 {
        let len = u32::from_be_bytes(rest[..4].try_into().ok()?) as usize;
        let kind = &rest[4..8];
        let body = rest.get(8..8usize.checked_add(len)?)?;
        if kind == b"IEND" {
            return None;
        }
        if kind == b"tEXt"
            && let Some(value) = body.strip_prefix(key.as_bytes()).and_then(|v| v.strip_prefix(b"\0"))
        {
            return Some(value);
        }
        rest = rest.get(12 + len..)?;
    }
    None
}

/// The modified time a thumbnail says it was made from, if it says. A thumbnail that doesn't
/// match its file is out of date and shows the wrong picture.
pub fn recorded_mtime(png: &[u8]) -> Option<i64> {
    std::str::from_utf8(text(png, "Thumb::MTime")?).ok()?.trim().parse().ok()
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
        assert_eq!(thumbnail_uri(Path::new("/a b/it's (1)+@,_-~.png")), "file:///a%20b/it's%20(1)+@,_-~.png");
        assert_eq!(thumbnail_uri(Path::new("/x#y%z?")), "file:///x%23y%25z%3F");
        assert_eq!(thumbnail_uri(Path::new("/é")), "file:///%C3%A9");
    }

    #[test]
    fn the_name_is_the_hash_of_the_uri() {
        // From the freedesktop spec's example, checked with md5sum.
        let found = candidates(Path::new("/c"), Path::new("/home/jens/photos/me.png"));
        assert_eq!(found[3], Path::new("/c/thumbnails/normal/c6ee772d9e49320e97ec29a7eb5b1697.png"));
        assert!(found[0].starts_with("/c/thumbnails/xx-large"));
    }

    #[test]
    fn gannets_own_places() {
        let photo = Path::new("/home/jens/photos/me.png");
        assert_eq!(normal(Path::new("/c"), photo), Path::new("/c/thumbnails/normal/c6ee772d9e49320e97ec29a7eb5b1697.png"));
        assert_eq!(failure(Path::new("/c"), photo), Path::new("/c/thumbnails/fail/gannet/c6ee772d9e49320e97ec29a7eb5b1697.png"));
        assert!(is_inside(Path::new("/c"), Path::new("/c/thumbnails/large/x.png")));
        assert!(!is_inside(Path::new("/c"), Path::new("/c/thumbnailsx/x.png")));
        assert!(!is_inside(Path::new("/c"), photo));
    }

    #[test]
    fn reads_any_text_entry() {
        let p = png(&[chunk(b"tEXt", b"Software\0Gannet 1"), chunk(b"tEXt", b"Thumb::MTimeX\0no"), chunk(b"tEXt", b"Thumb::MTime\x005"), chunk(b"IEND", b"")]);
        assert_eq!(text(&p, "Software"), Some(&b"Gannet 1"[..]));
        assert_eq!(text(&p, "Thumb::MTime"), Some(&b"5"[..]), "a longer key isn't this one");
        assert_eq!(text(&p, "Thumb::URI"), None);
    }

    #[test]
    fn reads_the_recorded_time() {
        let p = png(&[chunk(b"IHDR", &[0; 13]), chunk(b"tEXt", b"Software\0x"), chunk(b"tEXt", b"Thumb::MTime\x001234"), chunk(b"IEND", b"")]);
        assert_eq!(recorded_mtime(&p), Some(1234));
    }

    #[test]
    fn bad_or_missing_times_are_none() {
        assert_eq!(recorded_mtime(b"not a png"), None);
        assert_eq!(recorded_mtime(&png(&[chunk(b"IEND", b"")])), None);
        assert_eq!(recorded_mtime(&png(&[chunk(b"tEXt", b"Thumb::MTime\0soon")])), None);
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
