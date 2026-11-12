//! Icons, drawn from small vector paths and tinted from the theme. Being masks, they
//! follow the theme on their own and cost nothing to load at startup.

use std::collections::HashMap;

use swash::zeno::{Fill, Format, Mask, Transform};

use crate::canvas::Canvas;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Icon {
    Folder,
    FolderLink,
    File,
    FileLink,
    Image,
    Video,
    Audio,
    Archive,
    Code,
    Document,
    Back,
    Forward,
    Up,
    Home,
    Trash,
    Disk,
    ArrowUp,
    ArrowDown,
    Close,
    Eject,
    Lock,
    Download,
}

// All on a 16x16 grid, with every straight edge on a whole pixel, so they're sharp when drawn
// at 16 pixels or a whole multiple of it. Holes are cut with the even-odd rule.
/// A page with its corner folded, and whatever's cut out of it.
macro_rules! concat_file {
    ($rest:literal) => {
        concat!("M3 1h6v5h4v9H3V1Zm7 0 3 3h-3V1Z", $rest)
    };
}
const FILE: &str = concat_file!("");

fn path_of(i: Icon) -> &'static str {
    match i {
        Icon::Folder => "M1 3h6v2h8v8H1V3Z",
        Icon::FolderLink => "M1 3h6v2h8v8H1V3Zm4 5h3V6l3 3-3 3v-2H5V8Z",
        Icon::File => FILE,
        Icon::FileLink => concat_file!("M5 10h3V8l3 3-3 3v-2H5v-2Z"),
        Icon::Image => "M1 3h14v10H1V3Zm1 1v8h12V4H2Zm1 7 3-4 2 2 2-1 3 3H3Zm8-6h2v2h-2V5Z",
        Icon::Video => "M1 3h14v10H1V3Zm5 2v6l5-3-5-3Z",
        Icon::Audio => "M6 4 14 2v8.5a2 2 0 1 1-1-1.73V5L7 6.5v6a2 2 0 1 1-1-1.73V4Z",
        Icon::Archive => "M2 2h12v3H2V2Zm1 4h10v8H3V6Zm3 2v1h4V8H6Z",
        Icon::Code => concat_file!("M7 8h1v1h-1ZM6 9h1v1h-1ZM5 10h1v1h-1ZM6 11h1v1h-1ZM7 12h1v1h-1ZM9 8h1v1h-1ZM10 9h1v1h-1ZM11 10h1v1h-1ZM10 11h1v1h-1ZM9 12h1v1h-1Z"),
        Icon::Document => concat_file!("M5 7h6v1H5V7Zm0 2h6v1H5V9Zm0 2h4v1H5v-1Z"),
        Icon::Back => "M10 2l2 2-4 4 4 4-2 2-6-6 6-6Z",
        Icon::Forward => "M6 2 4 4l4 4-4 4 2 2 6-6-6-6Z",
        Icon::Up => "M2 10l2 2 4-4 4 4 2-2-6-6-6 6Z",
        Icon::Home => "M8 1l7 7h-2v7h-3v-5H6v5H3V8H1l7-7Z",
        Icon::Trash => "M6 1h4v1h4v2H2V2h4V1ZM4 5h8v10H4V5Zm2 2v6h1V7H6Zm3 0v6h1V7H9Z",
        Icon::Disk => "M1 4h14v8H1V4Zm10 3v2h2V7h-2Z",
        Icon::Eject => "M8 3l6 6H2l6-6ZM2 11h12v2H2v-2Z",
        Icon::Lock => "M5 7V5a3 3 0 0 1 6 0v2H9V5a1 1 0 0 0-2 0v2H5ZM3 7h10v8H3V7Z",
        // An arrow down into a tray.
        Icon::Download => "M7 1h2v6h3l-4 4-4-4h3V1ZM1 10h2v3h10v-3h2v5H1v-5Z",
        Icon::ArrowUp => "M8 5 12 10H4L8 5Z",
        Icon::ArrowDown => "M8 11 4 6h8L8 11Z",
        Icon::Close => "M4 3 8 7l4-4 1 1-4 4 4 4-1 1-4-4-4 4-1-1 4-4-4-4 1-1Z",
    }
}

#[derive(Default)]
pub struct Icons {
    cache: HashMap<(Icon, i32), (Vec<u8>, i32)>,
}

impl Icons {
    pub fn draw(&mut self, canvas: &mut Canvas, icon: Icon, x: i32, y: i32, size: i32, color: u32) {
        let (mask, s) = self.cache.entry((icon, size)).or_insert_with(|| {
            let scale = size as f32 / 16.0;
            let (data, _) = Mask::new(path_of(icon)).style(Fill::EvenOdd).size(size as u32, size as u32).format(Format::Alpha).transform(Some(Transform::scale(scale, scale))).render();
            (data, size)
        });
        canvas.mask(mask, *s, *s, x, y, color);
    }
}

/// The icon for a file, by extension. Reading contents would mean touching the disk.
pub fn icon_for_name(name: &[u8]) -> Icon {
    let Some(dot) = name.iter().rposition(|&b| b == b'.') else { return Icon::File };
    if dot == 0 {
        return Icon::File;
    }
    let ext = &name[dot + 1..];
    if ext.len() > 8 {
        return Icon::File;
    }
    let mut buf = [0u8; 8];
    for (o, b) in buf.iter_mut().zip(ext) {
        *o = b.to_ascii_lowercase();
    }
    match &buf[..ext.len()] {
        b"png" | b"jpg" | b"jpeg" | b"gif" | b"webp" | b"svg" | b"bmp" | b"tif" | b"tiff" | b"heic" | b"avif" | b"ico" | b"raw" | b"cr2" | b"nef" => Icon::Image,
        b"mp4" | b"mkv" | b"webm" | b"mov" | b"avi" | b"m4v" | b"wmv" | b"flv" | b"mpg" | b"mpeg" => Icon::Video,
        b"mp3" | b"flac" | b"ogg" | b"opus" | b"wav" | b"m4a" | b"aac" | b"wma" | b"aiff" => Icon::Audio,
        b"zip" | b"tar" | b"gz" | b"tgz" | b"xz" | b"zst" | b"bz2" | b"7z" | b"rar" | b"iso" | b"img" | b"deb" | b"rpm" => Icon::Archive,
        b"rs" | b"c" | b"h" | b"cpp" | b"hpp" | b"py" | b"js" | b"ts" | b"tsx" | b"jsx" | b"go" | b"sh" | b"lua" | b"java" | b"rb" | b"php" | b"html" | b"css" | b"json" | b"toml" | b"yaml" | b"yml" | b"xml" | b"qml" | b"swift" => Icon::Code,
        b"pdf" | b"doc" | b"docx" | b"odt" | b"txt" | b"md" | b"rtf" | b"epub" | b"xls" | b"xlsx" | b"ods" | b"ppt" | b"pptx" | b"odp" | b"csv" | b"log" => Icon::Document,
        _ => Icon::File,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extensions_pick_icons_ignoring_case() {
        assert_eq!(icon_for_name(b"a.PNG"), Icon::Image);
        assert_eq!(icon_for_name(b"a.tar.gz"), Icon::Archive);
        assert_eq!(icon_for_name(b".bashrc"), Icon::File);
        assert_eq!(icon_for_name(b"noext"), Icon::File);
        assert_eq!(icon_for_name(b"a.averyveryverylongext"), Icon::File);
        assert_eq!(icon_for_name(b"bad\xff.rs"), Icon::Code);
    }

    #[test]
    fn every_icon_draws_something() {
        use Icon::*;
        for i in [Folder, FolderLink, File, FileLink, Image, Video, Audio, Archive, Code, Document, Back, Forward, Up, Home, Trash, Disk, ArrowUp, ArrowDown, Close, Download] {
            let mut px = vec![0u32; 32 * 32];
            let mut c = Canvas::new(&mut px, 32, 32);
            Icons::default().draw(&mut c, i, 0, 0, 32, 0xffffff);
            assert!(px.iter().any(|&p| p != 0), "{i:?} drew nothing");
        }
    }

    /// The icons drawn at 16 pixels are sharp there: their straight edges sit on whole pixels.
    /// A rectangle's inside at 16 pixels is solid, with no half-covered pixels along its edges.
    #[test]
    fn icons_drawn_at_16_pixels_have_whole_pixel_edges() {
        use Icon::*;
        for i in [Folder, FolderLink, File, FileLink, Image, Video, Archive, Code, Document, Home, Trash, Disk, Eject, Lock, Download] {
            let numbers = path_of(i).split(|c: char| !(c.is_ascii_digit() || c == '.')).filter(|n| !n.is_empty());
            // Arcs' radii can be anything: only Lock has them, and its are whole too.
            assert!(numbers.clone().all(|n| !n.contains('.')), "{i:?} has an edge between pixels: {}", path_of(i));
        }
        let mut px = vec![0u32; 16 * 16];
        let mut c = Canvas::new(&mut px, 16, 16);
        Icons::default().draw(&mut c, Disk, 0, 0, 16, 0xffffff);
        let partial = px.iter().filter(|&&p| p != 0 && p & 0xffffff != 0xffffff).count();
        assert_eq!(partial, 0, "a drive's outline is all whole pixels");
    }
}
