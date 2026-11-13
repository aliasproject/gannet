//! Pictures dragged in from a web browser. A browser doesn't hand over a file: it offers the
//! picture's web address as a `text/uri-list`, and Firefox and its relatives (Zen) also offer the
//! picture itself, as `image/png` or the like. Here that picture becomes a new file, named from
//! its address.

use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use crate::uri::decode_path;

/// Pictures a drop may offer, most wanted first, with the extension each is saved under.
const IMAGES: [(&str, &str); 8] = [("image/png", "png"), ("image/jpeg", "jpg"), ("image/webp", "webp"), ("image/gif", "gif"), ("image/avif", "avif"), ("image/svg+xml", "svg"), ("image/bmp", "bmp"), ("image/tiff", "tiff")];

/// The picture format to ask for, if a drop offers one.
pub fn image_mime(offered: &[String]) -> Option<&'static str> {
    IMAGES.iter().map(|(m, _)| *m).find(|m| offered.iter().any(|o| o == m))
}

/// The first web address in a `text/uri-list`, if there is one.
pub fn web_address(uri_list: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(uri_list);
    text.lines().map(str::trim).filter(|l| !l.starts_with('#')).find(|l| l.starts_with("https://") || l.starts_with("http://")).map(str::to_string)
}

/// A name for a picture from `address`: the last part of its path, with the extension its
/// format has, like `photo.jpg`. One that can't be a file name is `image` with that extension.
pub fn image_name(address: Option<&str>, mime: &str) -> OsString {
    let ext = IMAGES.iter().find(|(m, _)| *m == mime).map_or("png", |(_, e)| e);
    let last = address.and_then(|a| {
        let rest = a.split_once("://")?.1;
        let path = rest.split(['?', '#']).next()?;
        let (_, path) = path.split_once('/')?;
        let segment = path.rsplit('/').next()?;
        let name = decode_path(segment).into_os_string().into_vec();
        // A name is one part of a path, not hidden, with nothing a terminal would act on.
        let ok = !name.is_empty() && name[0] != b'.' && !name.contains(&b'/') && !name.contains(&0) && name.iter().all(|&b| b >= 0x20 && b != 0x7f);
        ok.then_some(name)
    });
    let mut name = last.unwrap_or_else(|| b"image".to_vec());
    // A name ending in dots would get one more before the extension.
    while name.len() > 1 && name.last() == Some(&b'.') {
        name.pop();
    }
    // The extension says what the file is, so it's the one for what was saved.
    let dot = name.iter().rposition(|&b| b == b'.').filter(|&i| i > 0);
    let has = dot.map(|i| name[i + 1..].to_ascii_lowercase());
    let same = has.as_deref().is_some_and(|e| e == ext.as_bytes() || (ext == "jpg" && e == b"jpeg") || (ext == "tiff" && e == b"tif"));
    // Another picture's extension, as for a JPEG the browser hands over as a PNG, is replaced
    // rather than followed by this one.
    let other_picture = has.as_deref().is_some_and(|e| IMAGES.iter().any(|(_, x)| x.as_bytes() == e) || e == b"jpeg" || e == b"tif");
    if let (Some(i), true, false) = (dot, other_picture, same) {
        name.truncate(i);
    }
    if !same {
        // Room for the extension within the 255-byte limit, cut at a character's start.
        let mut room = 255 - ext.len() - 1;
        while room < name.len() && room > 0 && name[room] & 0xc0 == 0x80 {
            room -= 1;
        }
        name.truncate(room);
        name.push(b'.');
        name.extend_from_slice(ext.as_bytes());
    } else if name.len() > 255 {
        name = b"image.".iter().chain(ext.as_bytes()).copied().collect();
    }
    OsString::from_vec(name)
}

/// Writes `bytes` to a new file called `name` in `dir`, or `name (2)`, `name (3)` and so on if
/// that's taken, never over an existing file. A file that fails part way is removed.
pub fn save_new(dir: &Path, name: &std::ffi::OsStr, bytes: &[u8]) -> io::Result<PathBuf> {
    let raw = name.as_bytes();
    let dot = raw.iter().rposition(|&b| b == b'.').filter(|&i| i > 0).unwrap_or(raw.len());
    let (stem, ext) = raw.split_at(dot);
    for n in 1u32..1000 {
        let mut candidate = stem.to_vec();
        if n > 1 {
            let suffix = format!(" ({n})");
            let mut room = 255usize.saturating_sub(suffix.len() + ext.len());
            while room < candidate.len() && room > 0 && candidate[room] & 0xc0 == 0x80 {
                room -= 1;
            }
            candidate.truncate(room);
            candidate.extend_from_slice(suffix.as_bytes());
        }
        candidate.extend_from_slice(ext);
        let path = dir.join(OsString::from_vec(candidate));
        let mut file = match fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        };
        return match file.write_all(bytes).and_then(|()| file.sync_all()) {
            Ok(()) => Ok(path),
            Err(e) => {
                let _ = fs::remove_file(&path);
                Err(e)
            }
        };
    }
    Err(io::Error::new(io::ErrorKind::AlreadyExists, "every name is taken"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::Scratch;

    #[test]
    fn the_picture_format_wanted_most_is_asked_for() {
        let offered: Vec<String> = ["text/uri-list", "text/x-moz-url", "image/jpeg", "image/png"].map(String::from).to_vec();
        assert_eq!(image_mime(&offered), Some("image/png"));
        assert_eq!(image_mime(&["text/uri-list".to_string()]), None);
    }

    #[test]
    fn the_web_address_is_found_among_comments_and_files() {
        assert_eq!(web_address(b"# from a page\r\nhttps://example.com/a/photo.jpg\r\n").as_deref(), Some("https://example.com/a/photo.jpg"));
        assert_eq!(web_address(b"file:///home/a/b.png\r\n"), None);
    }

    #[test]
    fn names_come_from_the_address_and_fit_the_format() {
        let name = |a: Option<&str>, m| image_name(a, m).into_string().unwrap();
        assert_eq!(name(Some("https://github.com/omacom/folio/raw/main/docs/columns.png?raw=true"), "image/png"), "columns.png");
        assert_eq!(name(Some("https://example.com/my%20photo.jpeg"), "image/jpeg"), "my photo.jpeg", "jpeg is a jpg");
        assert_eq!(name(Some("https://example.com/photo.webp"), "image/png"), "photo.png", "it's saved as what it is");
        assert_eq!(name(Some("https://example.com/photo.jpg"), "image/png"), "photo.png");
        assert_eq!(name(Some("https://example.com/notes.txt"), "image/png"), "notes.txt.png", "only a picture's extension is replaced");
        assert_eq!(name(Some("https://example.com/a."), "image/png"), "a.png", "no doubled dot");
        assert_eq!(name(Some("https://example.com/render"), "image/png"), "render.png");
        assert_eq!(name(Some("https://example.com/"), "image/gif"), "image.gif");
        assert_eq!(name(Some("https://example.com"), "image/png"), "image.png");
        assert_eq!(name(None, "image/png"), "image.png");
        assert_eq!(name(Some("https://example.com/a%2Fb.png"), "image/png"), "image.png", "no slash in a name");
        assert_eq!(name(Some("https://example.com/.bashrc"), "image/png"), "image.png", "not hidden");
        assert_eq!(name(Some("https://example.com/a%1Bb.png"), "image/png"), "image.png", "no control characters");
        let long = format!("https://example.com/{}.png", "é".repeat(200));
        let n = image_name(Some(&long), "image/png");
        assert!(n.len() <= 255 && n.to_str().is_some(), "cut within the limit, at a character");
    }

    #[test]
    fn a_picture_is_saved_beside_others_without_replacing_them() {
        let s = Scratch::new();
        let dir = s.dir("Downloads");
        s.file("Downloads/photo.png", b"old");
        let first = save_new(&dir, std::ffi::OsStr::new("photo.png"), b"new").unwrap();
        assert_eq!(first, dir.join("photo (2).png"));
        let second = save_new(&dir, std::ffi::OsStr::new("photo.png"), b"newer").unwrap();
        assert_eq!(second, dir.join("photo (3).png"));
        assert_eq!(fs::read(dir.join("photo.png")).unwrap(), b"old", "never over an existing file");
        assert_eq!(fs::read(&first).unwrap(), b"new");
        assert!(save_new(&s.path().join("missing"), std::ffi::OsStr::new("a.png"), b"x").is_err());
    }
}
