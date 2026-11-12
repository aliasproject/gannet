//! Icons, drawn from small vector paths and tinted from the theme. Being masks, they
//! follow the theme on their own and cost nothing to load at startup.

use std::collections::HashMap;

use swash::zeno::{Format, Mask, Transform};

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
    Computer,
    ArrowUp,
    ArrowDown,
    Close,
    Eject,
    Lock,
}

// All on a 16x16 grid.
const FILE: &str = "M4 1.5h5.5L13 5v9.5H3V1.5ZM9 2.8V5.5h2.7L9 2.8Z";

fn path_of(i: Icon) -> &'static str {
    match i {
        Icon::Folder => "M1.5 3h4.8l1.5 1.8h6.7v8.7h-13V3Z",
        Icon::FolderLink => "M1.5 3h4.8l1.5 1.8h6.7v8.7h-13V3Zm5 4v1.5H5v2.5h1.5V9.8h2V11.5L12 9 8.5 6.5V8H6.5V7Z",
        Icon::File => FILE,
        Icon::FileLink => "M4 1.5h5.5L13 5v9.5H3V1.5ZM6 8v1.2H5v2.6h1.2v-1.4H8v1.3L11 9.6 8 7.4v1.4H6V8Z",
        Icon::Image => "M2 3h12v10.5H2V3Zm1.3 1.3v6.2l2.7-3 2.2 2.3 1.6-1.4 2.9 2.9V4.3H3.3ZM10.5 5.2a1.2 1.2 0 1 1 0 2.4 1.2 1.2 0 0 1 0-2.4Z",
        Icon::Video => "M2 3h12v10.5H2V3Zm4.5 2.8v5l4.2-2.5-4.2-2.5Z",
        Icon::Audio => "M6 2.8 13.5 1.5v9.2a2 2 0 1 1-1.3-1.9V4L7.3 4.9v7.6a2 2 0 1 1-1.3-1.9V2.8Z",
        Icon::Archive => "M2 2.5h12v3H2v-3Zm.8 4H13.2v8H2.8v-8ZM6 8v1.3h4V8H6Z",
        Icon::Code => "M4 1.5h5.5L13 5v9.5H3V1.5ZM6.6 7.3 4.8 9.3l1.8 2 .9-.8-1.1-1.2 1.1-1.2-.9-.8Zm2.8 0-.9.8 1.1 1.2-1.1 1.2.9.8 1.8-2-1.8-2Z",
        Icon::Document => "M4 1.5h5.5L13 5v9.5H3V1.5ZM5 7v1.2h6V7H5Zm0 2.5v1.2h6V9.5H5Zm0 2.5v1.2h4V12H5Z",
        Icon::Back => "M10.5 2.5 12 4 7.8 8 12 12 10.5 13.5 5 8l5.5-5.5Z",
        Icon::Forward => "M5.5 2.5 4 4l4.2 4L4 12l1.5 1.5L11 8 5.5 2.5Z",
        Icon::Up => "M2.5 10.5 4 12l4-4.2L12 12l1.5-1.5L8 5l-5.5 5.5Z",
        Icon::Home => "M8 1.8 15 8h-2v6.2H9.5V10h-3v4.2H3V8H1L8 1.8Z",
        Icon::Trash => "M6 1.5h4l.6 1.5H14v1.5H2V3h3.4L6 1.5ZM3.2 5.5h9.6l-.7 9H3.9l-.7-9Z",
        Icon::Disk => "M2 4h12v8H2V4Zm9.5 3a1 1 0 1 0 0 2 1 1 0 0 0 0-2Z",
        Icon::Eject => "M8 3.5L13 9.5H3L8 3.5ZM3 11h10v1.8H3V11Z",
        Icon::Lock => "M5 7V5.2a3 3 0 0 1 6 0V7h1.2v6.8H3.8V7H5Zm1.5 0h3V5.2a1.5 1.5 0 0 0-3 0V7Z",
        Icon::Computer => "M1.5 2.5h13v8.5h-13V2.5ZM3 4v5.5h10V4H3Zm2.5 8.5h5v1.5h-5v-1.5Z",
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
            let (data, _) = Mask::new(path_of(icon)).size(size as u32, size as u32).format(Format::Alpha).transform(Some(Transform::scale(scale, scale))).render();
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
        for i in [Folder, FolderLink, File, FileLink, Image, Video, Audio, Archive, Code, Document, Back, Forward, Up, Home, Trash, Disk, Computer, ArrowUp, ArrowDown, Close] {
            let mut px = vec![0u32; 32 * 32];
            let mut c = Canvas::new(&mut px, 32, 32);
            Icons::default().draw(&mut c, i, 0, 0, 32, 0xffffff);
            assert!(px.iter().any(|&p| p != 0), "{i:?} drew nothing");
        }
    }
}
