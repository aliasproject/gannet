//! File lists on the clipboard, in the formats file managers exchange.
//!
//! `x-special/gnome-copied-files` says whether it's a cut: a line of `copy` or `cut`,
//! then URIs. `text/uri-list` is one URI per line, and always means copy.

use std::path::{Path, PathBuf};

use crate::uri::{from_file_uri, to_file_uri};

pub const GNOME: &str = "x-special/gnome-copied-files";
pub const URI_LIST: &str = "text/uri-list";
pub const TEXT: &str = "text/plain;charset=utf-8";
pub const TEXT_PLAIN: &str = "text/plain";
pub const UTF8_STRING: &str = "UTF8_STRING";
/// KDE's way of saying a uri-list is a cut: this holds "1".
pub const KDE_CUT: &str = "application/x-kde-cutselection";
/// Marks data read as a uri-list that KDE's flag said was a cut. Internal to the app.
pub const URI_LIST_CUT: &str = "text/uri-list;cut";

/// The formats to offer for these paths, by MIME type.
pub fn encode(paths: &[PathBuf], cut: bool) -> Vec<(&'static str, Vec<u8>)> {
    let uris: Vec<String> = paths.iter().map(|p| to_file_uri(p)).collect();
    let gnome = format!("{}\n{}", if cut { "cut" } else { "copy" }, uris.join("\n"));
    let list = format!("{}\r\n", uris.join("\r\n"));
    let text = paths.iter().map(|p| p.to_string_lossy().into_owned()).collect::<Vec<_>>().join("\n");
    vec![
        (GNOME, gnome.into_bytes()),
        (URI_LIST, list.into_bytes()),
        (KDE_CUT, if cut { b"1".to_vec() } else { b"0".to_vec() }),
        (TEXT, text.clone().into_bytes()),
        (TEXT_PLAIN, text.clone().into_bytes()),
        (UTF8_STRING, text.into_bytes()),
    ]
}

/// The formats to offer other programs in a drag. Whether it's a move is agreed separately,
/// so the clipboard's cut markers don't belong here.
pub fn drag_formats(paths: &[PathBuf]) -> Vec<(&'static str, Vec<u8>)> {
    encode(paths, false).into_iter().filter(|(m, _)| *m != GNOME && *m != KDE_CUT).collect()
}

/// Whether every entry in a dropped uri-list is one of these local paths. If not, the drop
/// can't be called a complete success, whatever happens to the local ones.
pub fn uri_list_is_all_local(data: &[u8], paths: &[PathBuf]) -> bool {
    let text = String::from_utf8_lossy(data);
    text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).count() == paths.len()
}

/// Says a drag comes from another Gannet window, and what that window knows that a receiver
/// can't otherwise see. It's all in the format's name, because a window can read an offer's
/// format names as soon as a drag arrives, with no waiting on the other program. Other programs
/// ignore a format they don't know, and they're only ever offered what the actions allow.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DragMarker {
    /// The source folder's device.
    pub device: u64,
    /// Whether its items can be deleted.
    pub can_delete: bool,
    /// The keys held when the drag left its window, for a receiver without the keyboard.
    pub ctrl: bool,
    pub shift: bool,
}

const DRAG_MARKER: &str = "application/x-gannet-drag";

impl DragMarker {
    pub fn mime(&self) -> String {
        format!("{DRAG_MARKER};device={};delete={};ctrl={};shift={}", self.device, u8::from(self.can_delete), u8::from(self.ctrl), u8::from(self.shift))
    }

    /// Reads a format name. Anything other than exactly what `mime` writes isn't a marker.
    pub fn parse(mime: &str) -> Option<Self> {
        let flag = |v: &str| match v {
            "0" => Some(false),
            "1" => Some(true),
            _ => None,
        };
        let mut parts = mime.split(';');
        (parts.next()? == DRAG_MARKER).then_some(())?;
        let device = parts.next()?.strip_prefix("device=")?.parse().ok()?;
        let can_delete = flag(parts.next()?.strip_prefix("delete=")?)?;
        let ctrl = flag(parts.next()?.strip_prefix("ctrl=")?)?;
        let shift = flag(parts.next()?.strip_prefix("shift=")?)?;
        parts.next().is_none().then_some(Self { device, can_delete, ctrl, shift })
    }
}

/// Which offered format to ask for when pasting files.
pub fn preferred_file_mime(offered: &[String]) -> Option<&'static str> {
    [GNOME, URI_LIST].into_iter().find(|m| offered.iter().any(|o| o == m))
}

/// Which offered format to ask for when pasting into a text field.
pub fn preferred_text_mime(offered: &[String]) -> Option<&'static str> {
    [TEXT, UTF8_STRING, TEXT_PLAIN].into_iter().find(|m| offered.iter().any(|o| o == m))
}

/// Reads a clipboard pipe, refusing anything over `max` rather than acting on a cut-off
/// list, whose last path could name some other file.
pub fn read_limited(reader: impl std::io::Read, max: u64) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut data = Vec::new();
    reader.take(max + 1).read_to_end(&mut data).ok()?;
    (data.len() as u64 <= max).then_some(data)
}

/// Local files in the data, and whether it was a cut. Anything that isn't a local path is left out.
pub fn parse(mime: &str, data: &[u8]) -> (Vec<PathBuf>, bool) {
    let text = String::from_utf8_lossy(data);
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
    let mut cut = mime == URI_LIST_CUT;
    if mime == GNOME {
        match lines.next() {
            Some("cut") => cut = true,
            Some("copy") => {}
            _ => return (Vec::new(), false),
        }
    }
    // `#` starts a comment in a uri-list.
    let paths = lines.filter(|l| !l.starts_with('#')).filter_map(from_file_uri).filter(|p| Path::is_absolute(p)).collect();
    (paths, cut)
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_list_with_something_remote_isnt_all_local() {
        let data = b"file:///a\r\nsmb://host/b\r\n";
        let (paths, _) = parse(URI_LIST, data);
        assert_eq!(paths, [PathBuf::from("/a")]);
        assert!(!uri_list_is_all_local(data, &paths));
        let data = b"# a comment\r\nfile:///a\r\n";
        let (paths, _) = parse(URI_LIST, data);
        assert!(uri_list_is_all_local(data, &paths));
    }

    #[test]
    fn drags_offer_a_uri_list_and_no_cut_markers() {
        let f = drag_formats(&[PathBuf::from("/a b/c")]);
        let mimes: Vec<&str> = f.iter().map(|(m, _)| *m).collect();
        assert!(mimes.contains(&URI_LIST));
        assert!(!mimes.contains(&GNOME) && !mimes.contains(&KDE_CUT));
        let list = &f.iter().find(|(m, _)| *m == URI_LIST).unwrap().1;
        assert_eq!(list.as_slice(), b"file:///a%20b/c\r\n");
    }

    use super::*;

    fn get<'a>(v: &'a [(&str, Vec<u8>)], m: &str) -> &'a [u8] {
        &v.iter().find(|(k, _)| *k == m).unwrap().1
    }

    #[test]
    fn matches_what_nautilus_writes() {
        let v = encode(&["/a b/c.txt".into(), "/d".into()], false);
        assert_eq!(get(&v, GNOME), b"copy\nfile:///a%20b/c.txt\nfile:///d");
        assert_eq!(get(&v, URI_LIST), b"file:///a%20b/c.txt\r\nfile:///d\r\n");
        assert_eq!(get(&v, TEXT), b"/a b/c.txt\n/d");
        let v = encode(&["/d".into()], true);
        assert!(get(&v, GNOME).starts_with(b"cut\n"));
    }

    #[test]
    fn round_trips() {
        let paths: Vec<PathBuf> = vec!["/x y/é".into(), "/z".into()];
        for cut in [false, true] {
            let v = encode(&paths, cut);
            assert_eq!(parse(GNOME, get(&v, GNOME)), (paths.clone(), cut));
            assert_eq!(parse(URI_LIST, get(&v, URI_LIST)), (paths.clone(), false));
        }
    }

    #[test]
    fn junk_and_remote_uris_are_left_out() {
        assert_eq!(parse(GNOME, b"not a verb\nfile:///a"), (vec![], false));
        assert_eq!(parse(URI_LIST, b"# comment\nhttps://x/y\nfile:///a\nfile://host/b\nrelative\n"), (vec![PathBuf::from("/a")], false));
        assert_eq!(parse(URI_LIST, b"\xff\xfe"), (vec![], false));
        assert_eq!(parse(GNOME, b""), (vec![], false));
    }

    #[test]
    fn kde_programs_can_read_and_write_a_cut() {
        assert_eq!(get(&encode(&["/d".into()], true), KDE_CUT), b"1");
        assert_eq!(get(&encode(&["/d".into()], false), KDE_CUT), b"0");
        assert_eq!(parse(URI_LIST_CUT, b"file:///a\r\n"), (vec![PathBuf::from("/a")], true));
    }

    #[test]
    fn oversized_clipboard_data_is_refused_not_cut_off() {
        assert_eq!(read_limited(&b"12345"[..], 5), Some(b"12345".to_vec()));
        assert_eq!(read_limited(&b"123456"[..], 5), None);
    }

    #[test]
    fn preference_order() {
        let o = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(preferred_file_mime(&o(&[URI_LIST, GNOME])), Some(GNOME));
        assert_eq!(preferred_file_mime(&o(&[URI_LIST])), Some(URI_LIST));
        assert_eq!(preferred_file_mime(&o(&["text/plain"])), None);
        assert_eq!(preferred_text_mime(&o(&["UTF8_STRING", "text/plain"])), Some("UTF8_STRING"));
    }

    #[test]
    fn a_drag_marker_survives_its_format_name() {
        for (can_delete, ctrl, shift) in [(false, false, false), (true, false, false), (false, true, false), (false, false, true), (true, true, true)] {
            let m = DragMarker { device: u64::MAX, can_delete, ctrl, shift };
            assert_eq!(DragMarker::parse(&m.mime()), Some(m));
        }
    }

    #[test]
    fn only_an_exact_format_name_is_a_drag_marker() {
        let good = "application/x-gannet-drag;device=7;delete=1;ctrl=0;shift=0";
        assert!(DragMarker::parse(good).is_some());
        for bad in [
            URI_LIST,
            "application/x-gannet-drag",
            "application/x-gannet-dragged;device=7;delete=1;ctrl=0;shift=0",
            "application/x-gannet-drag;device=x;delete=1;ctrl=0;shift=0",
            "application/x-gannet-drag;device=-7;delete=1;ctrl=0;shift=0",
            "application/x-gannet-drag;device=7;delete=2;ctrl=0;shift=0",
            "application/x-gannet-drag;delete=1;device=7;ctrl=0;shift=0",
            "application/x-gannet-drag;device=7;delete=1;ctrl=0",
            "application/x-gannet-drag;device=7;delete=1;ctrl=0;shift=0;more=1",
        ] {
            assert_eq!(DragMarker::parse(bad), None, "{bad}");
        }
    }
}
