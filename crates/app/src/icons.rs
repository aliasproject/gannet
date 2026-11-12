//! Icons, Lucide's, drawn from small vector paths and tinted from the theme. Being masks, they
//! follow the theme on their own and cost nothing to load at startup.

use std::collections::HashMap;

use swash::zeno::{Cap, Format, Join, Mask, Stroke, Transform};

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
    /// The `>` between the path bar's folders.
    Crumb,
}

// Lucide's icons (https://lucide.dev, ISC; see LICENSES/lucide.txt), named in each arm: outlines
// on a 24-unit grid, stroked 2 units wide with round ends and corners, as Lucide draws them.
fn path_of(i: Icon) -> &'static str {
    match i {
        // folder
        Icon::Folder => "M20 20a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.9a2 2 0 0 1-1.69-.9L9.6 3.9A2 2 0 0 0 7.93 3H4a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2Z",
        // folder-symlink
        Icon::FolderLink => "M2 9V5a2 2 0 0 1 2-2h3.9a2 2 0 0 1 1.69.9l.81 1.2a2 2 0 0 0 1.67.9H20a2 2 0 0 1 2 2v10a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2v-3a2 2 0 0 1 2-2h7M8 16l3-3-3-3",
        // file
        Icon::File => "M15 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V7ZM14 2v4a2 2 0 0 0 2 2h4",
        // file-symlink
        Icon::FileLink => "M10 18l3-3-3-3M14 2v4a2 2 0 0 0 2 2h4M4 11V4a2 2 0 0 1 2-2h9l5 5v13a2 2 0 0 1-2 2H6a2 2 0 0 1-2-2v-3a2 2 0 0 1 2-2h7",
        // image
        Icon::Image => "M5 3h14a2 2 0 0 1 2 2v14a2 2 0 0 1 -2 2h-14a2 2 0 0 1 -2 -2v-14a2 2 0 0 1 2 -2ZM7 9a2 2 0 1 0 4 0a2 2 0 1 0 -4 0ZM21 15l-3.086-3.086a2 2 0 0 0-2.828 0L6 21",
        // film
        Icon::Video => "M5 3h14a2 2 0 0 1 2 2v14a2 2 0 0 1 -2 2h-14a2 2 0 0 1 -2 -2v-14a2 2 0 0 1 2 -2ZM7 3v18M3 7.5h4M3 12h18M3 16.5h4M17 3v18M17 7.5h4M17 16.5h4",
        // music
        Icon::Audio => "M9 18V5l12-2v13M3 18a3 3 0 1 0 6 0a3 3 0 1 0 -6 0ZM15 16a3 3 0 1 0 6 0a3 3 0 1 0 -6 0Z",
        // archive
        Icon::Archive => "M3 3h18a1 1 0 0 1 1 1v3a1 1 0 0 1 -1 1h-18a1 1 0 0 1 -1 -1v-3a1 1 0 0 1 1 -1ZM4 8v11a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8M10 12h4",
        // code
        Icon::Code => "M16 18L22 12L16 6M8 6L2 12L8 18",
        // file-text
        Icon::Document => "M15 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V7ZM14 2v4a2 2 0 0 0 2 2h4M10 9H8M16 13H8M16 17H8",
        // chevron-left
        Icon::Back => "M15 18l-6-6 6-6",
        // chevron-right
        Icon::Forward => "M9 18l6-6-6-6",
        // chevron-up
        Icon::Up => "M18 15l-6-6-6 6",
        // house
        Icon::Home => "M15 21v-8a1 1 0 0 0-1-1h-4a1 1 0 0 0-1 1v8M3 10a2 2 0 0 1 .709-1.528l7-5.999a2 2 0 0 1 2.582 0l7 5.999A2 2 0 0 1 21 10v9a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z",
        // trash-2
        Icon::Trash => "M3 6h18M19 6v14c0 1-1 2-2 2H7c-1 0-2-1-2-2V6M8 6V4c0-1 1-2 2-2h4c1 0 2 1 2 2v2M10 11L10 17M14 11L14 17",
        // hard-drive
        Icon::Disk => "M22 12L2 12M5.45 5.11 2 12v6a2 2 0 0 0 2 2h16a2 2 0 0 0 2-2v-6l-3.45-6.89A2 2 0 0 0 16.76 4H7.24a2 2 0 0 0-1.79 1.11zM6 16L6.01 16M10 16L10.01 16",
        // Lucide has no eject: a triangle over a bar, drawn in its style.
        Icon::Eject => "M5 14 12 6l7 8ZM5 18h14",
        // lock
        Icon::Lock => "M5 11h14a2 2 0 0 1 2 2v7a2 2 0 0 1 -2 2h-14a2 2 0 0 1 -2 -2v-7a2 2 0 0 1 2 -2ZM7 11V7a5 5 0 0 1 10 0v4",
        // download
        Icon::Download => "M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4M7 10L12 15L17 10M12 15L12 3",
        // chevron-up
        Icon::ArrowUp => "M18 15l-6-6-6 6",
        // chevron-down
        Icon::ArrowDown => "M6 9l6 6 6-6",
        // x
        Icon::Close => "M18 6 6 18M6 6l12 12",
        // chevron-right
        Icon::Crumb => "M9 18l6-6-6-6",
    }
}

/// The grid Lucide draws on.
const GRID: f32 = 24.0;
/// Lucide's stroke, in grid units.
const STROKE: f32 = 2.0;
/// The thinnest a stroke gets, in pixels, so the small marks drawn at 10 to 14 pixels don't fade.
const MIN_STROKE_PX: f32 = 1.25;

#[derive(Default)]
pub struct Icons {
    cache: HashMap<(Icon, i32), (Vec<u8>, i32)>,
}

impl Icons {
    pub fn draw(&mut self, canvas: &mut Canvas, icon: Icon, x: i32, y: i32, size: i32, color: u32) {
        let (mask, s) = self.cache.entry((icon, size)).or_insert_with(|| {
            let scale = size as f32 / GRID;
            let width = STROKE.max(MIN_STROKE_PX / scale);
            let mut stroke = Stroke::new(width);
            stroke.cap(Cap::Round).join(Join::Round);
            let (data, _) = Mask::new(path_of(icon)).style(stroke).size(size as u32, size as u32).format(Format::Alpha).transform(Some(Transform::scale(scale, scale))).render();
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
        for i in [Folder, FolderLink, File, FileLink, Image, Video, Audio, Archive, Code, Document, Back, Forward, Up, Home, Trash, Disk, ArrowUp, ArrowDown, Close, Download, Crumb, Eject, Lock] {
            let mut px = vec![0u32; 32 * 32];
            let mut c = Canvas::new(&mut px, 32, 32);
            Icons::default().draw(&mut c, i, 0, 0, 32, 0xffffff);
            assert!(px.iter().any(|&p| p != 0), "{i:?} drew nothing");
        }
    }
}
