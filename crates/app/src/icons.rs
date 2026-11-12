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
    ArrowUp,
    ArrowDown,
    Close,
    Eject,
    Lock,
    Download,
    /// The `>` between the path bar's folders.
    Crumb,
}

// Each drawn pixel by pixel on a 16x16 grid: every pixel is fully on or off, so they're sharp
// at 16 pixels and any whole multiple of it, with no smoothing to blur them. Each fills about
// the same 14x14 area, so none looks smaller or squashed beside the others.

fn path_of(i: Icon) -> &'static str {
    match i {
        Icon::Folder => "M1 3h6v1h-6ZM1 4h6v1h-6ZM1 5h14v1h-14ZM1 6h14v1h-14ZM1 7h14v1h-14ZM1 8h14v1h-14ZM1 9h14v1h-14ZM1 10h14v1h-14ZM1 11h14v1h-14ZM1 12h14v1h-14Z",
        Icon::FolderLink => "M1 3h6v1h-6ZM1 4h6v1h-6ZM1 5h14v1h-14ZM1 6h7v1h-7ZM9 6h6v1h-6ZM1 7h7v1h-7ZM10 7h5v1h-5ZM1 8h3v1h-3ZM11 8h4v1h-4ZM1 9h3v1h-3ZM11 9h4v1h-4ZM1 10h7v1h-7ZM10 10h5v1h-5ZM1 11h7v1h-7ZM9 11h6v1h-6ZM1 12h14v1h-14Z",
        Icon::File => "M3 1h6v1h-6ZM10 1h1v1h-1ZM3 2h6v1h-6ZM10 2h2v1h-2ZM3 3h6v1h-6ZM10 3h3v1h-3ZM3 4h6v1h-6ZM3 5h10v1h-10ZM3 6h10v1h-10ZM3 7h10v1h-10ZM3 8h10v1h-10ZM3 9h10v1h-10ZM3 10h10v1h-10ZM3 11h10v1h-10ZM3 12h10v1h-10ZM3 13h10v1h-10ZM3 14h10v1h-10Z",
        Icon::FileLink => "M3 1h6v1h-6ZM10 1h1v1h-1ZM3 2h6v1h-6ZM10 2h2v1h-2ZM3 3h6v1h-6ZM10 3h3v1h-3ZM3 4h6v1h-6ZM3 5h10v1h-10ZM3 6h10v1h-10ZM3 7h10v1h-10ZM3 8h6v1h-6ZM10 8h3v1h-3ZM3 9h6v1h-6ZM11 9h2v1h-2ZM3 10h2v1h-2ZM12 10h1v1h-1ZM3 11h2v1h-2ZM12 11h1v1h-1ZM3 12h6v1h-6ZM11 12h2v1h-2ZM3 13h6v1h-6ZM10 13h3v1h-3ZM3 14h10v1h-10Z",
        Icon::Image => "M1 2h14v1h-14ZM1 3h1v1h-1ZM14 3h1v1h-1ZM1 4h1v1h-1ZM10 4h2v1h-2ZM14 4h1v1h-1ZM1 5h1v1h-1ZM10 5h2v1h-2ZM14 5h1v1h-1ZM1 6h1v1h-1ZM14 6h1v1h-1ZM1 7h1v1h-1ZM14 7h1v1h-1ZM1 8h1v1h-1ZM5 8h1v1h-1ZM14 8h1v1h-1ZM1 9h1v1h-1ZM4 9h3v1h-3ZM10 9h1v1h-1ZM14 9h1v1h-1ZM1 10h1v1h-1ZM3 10h5v1h-5ZM9 10h3v1h-3ZM14 10h1v1h-1ZM1 11h14v1h-14ZM1 12h14v1h-14ZM1 13h14v1h-14Z",
        Icon::Video => "M1 2h14v1h-14ZM1 3h14v1h-14ZM1 4h5v1h-5ZM7 4h8v1h-8ZM1 5h5v1h-5ZM8 5h7v1h-7ZM1 6h5v1h-5ZM9 6h6v1h-6ZM1 7h5v1h-5ZM10 7h5v1h-5ZM1 8h5v1h-5ZM10 8h5v1h-5ZM1 9h5v1h-5ZM9 9h6v1h-6ZM1 10h5v1h-5ZM8 10h7v1h-7ZM1 11h5v1h-5ZM7 11h8v1h-8ZM1 12h14v1h-14ZM1 13h14v1h-14Z",
        Icon::Audio => "M6 2.8 13.5 1.5v9.2a2 2 0 1 1-1.3-1.9V4L7.3 4.9v7.6a2 2 0 1 1-1.3-1.9V2.8Z",
        Icon::Archive => "M1 2h14v1h-14ZM1 3h14v1h-14ZM1 4h14v1h-14ZM2 6h12v1h-12ZM2 7h12v1h-12ZM2 8h4v1h-4ZM10 8h4v1h-4ZM2 9h12v1h-12ZM2 10h12v1h-12ZM2 11h12v1h-12ZM2 12h12v1h-12ZM2 13h12v1h-12Z",
        Icon::Code => "M3 1h6v1h-6ZM10 1h1v1h-1ZM3 2h6v1h-6ZM10 2h2v1h-2ZM3 3h6v1h-6ZM10 3h3v1h-3ZM3 4h6v1h-6ZM3 5h10v1h-10ZM3 6h10v1h-10ZM3 7h10v1h-10ZM3 8h3v1h-3ZM7 8h2v1h-2ZM10 8h3v1h-3ZM3 9h2v1h-2ZM6 9h4v1h-4ZM11 9h2v1h-2ZM3 10h1v1h-1ZM5 10h6v1h-6ZM12 10h1v1h-1ZM3 11h2v1h-2ZM6 11h4v1h-4ZM11 11h2v1h-2ZM3 12h3v1h-3ZM7 12h2v1h-2ZM10 12h3v1h-3ZM3 13h10v1h-10ZM3 14h10v1h-10Z",
        Icon::Document => "M3 1h6v1h-6ZM10 1h1v1h-1ZM3 2h6v1h-6ZM10 2h2v1h-2ZM3 3h6v1h-6ZM10 3h3v1h-3ZM3 4h6v1h-6ZM3 5h10v1h-10ZM3 6h10v1h-10ZM3 7h2v1h-2ZM11 7h2v1h-2ZM3 8h10v1h-10ZM3 9h2v1h-2ZM11 9h2v1h-2ZM3 10h10v1h-10ZM3 11h2v1h-2ZM9 11h4v1h-4ZM3 12h10v1h-10ZM3 13h10v1h-10ZM3 14h10v1h-10Z",
        Icon::Back => "M9 2h3v1h-3ZM8 3h3v1h-3ZM7 4h3v1h-3ZM6 5h3v1h-3ZM5 6h3v1h-3ZM4 7h3v1h-3ZM4 8h3v1h-3ZM5 9h3v1h-3ZM6 10h3v1h-3ZM7 11h3v1h-3ZM8 12h3v1h-3ZM9 13h3v1h-3Z",
        Icon::Forward => "M4 2h3v1h-3ZM5 3h3v1h-3ZM6 4h3v1h-3ZM7 5h3v1h-3ZM8 6h3v1h-3ZM9 7h3v1h-3ZM9 8h3v1h-3ZM8 9h3v1h-3ZM7 10h3v1h-3ZM6 11h3v1h-3ZM5 12h3v1h-3ZM4 13h3v1h-3Z",
        Icon::Up => "M7 4h2v1h-2ZM6 5h4v1h-4ZM5 6h6v1h-6ZM4 7h3v1h-3ZM9 7h3v1h-3ZM3 8h3v1h-3ZM10 8h3v1h-3ZM2 9h3v1h-3ZM11 9h3v1h-3ZM2 10h2v1h-2ZM12 10h2v1h-2ZM2 11h1v1h-1ZM13 11h1v1h-1Z",
        Icon::Home => "M7 1h2v1h-2ZM6 2h4v1h-4ZM5 3h6v1h-6ZM4 4h8v1h-8ZM3 5h10v1h-10ZM2 6h12v1h-12ZM1 7h14v1h-14ZM3 8h10v1h-10ZM3 9h10v1h-10ZM3 10h3v1h-3ZM10 10h3v1h-3ZM3 11h3v1h-3ZM10 11h3v1h-3ZM3 12h3v1h-3ZM10 12h3v1h-3ZM3 13h3v1h-3ZM10 13h3v1h-3ZM3 14h3v1h-3ZM10 14h3v1h-3Z",
        Icon::Trash => "M6 1h4v1h-4ZM2 2h12v1h-12ZM2 3h12v1h-12ZM4 5h8v1h-8ZM4 6h8v1h-8ZM4 7h2v1h-2ZM7 7h2v1h-2ZM10 7h2v1h-2ZM4 8h2v1h-2ZM7 8h2v1h-2ZM10 8h2v1h-2ZM4 9h2v1h-2ZM7 9h2v1h-2ZM10 9h2v1h-2ZM4 10h2v1h-2ZM7 10h2v1h-2ZM10 10h2v1h-2ZM4 11h2v1h-2ZM7 11h2v1h-2ZM10 11h2v1h-2ZM4 12h2v1h-2ZM7 12h2v1h-2ZM10 12h2v1h-2ZM4 13h8v1h-8ZM4 14h8v1h-8Z",
        Icon::Disk => "M1 4h14v1h-14ZM1 5h14v1h-14ZM1 6h14v1h-14ZM1 7h10v1h-10ZM13 7h2v1h-2ZM1 8h10v1h-10ZM13 8h2v1h-2ZM1 9h14v1h-14ZM1 10h14v1h-14ZM1 11h14v1h-14Z",
        Icon::Eject => "M7 3h2v1h-2ZM6 4h4v1h-4ZM5 5h6v1h-6ZM4 6h8v1h-8ZM3 7h10v1h-10ZM2 8h12v1h-12ZM2 10h12v1h-12ZM2 11h12v1h-12Z",
        Icon::Lock => "M6 1h4v1h-4ZM5 2h2v1h-2ZM9 2h2v1h-2ZM5 3h2v1h-2ZM9 3h2v1h-2ZM5 4h2v1h-2ZM9 4h2v1h-2ZM5 5h2v1h-2ZM9 5h2v1h-2ZM5 6h2v1h-2ZM9 6h2v1h-2ZM3 7h10v1h-10ZM3 8h10v1h-10ZM3 9h10v1h-10ZM3 10h10v1h-10ZM3 11h10v1h-10ZM3 12h10v1h-10ZM3 13h10v1h-10ZM3 14h10v1h-10Z",
        // An arrow down into a tray.
        Icon::Download => "M7 1h2v1h-2ZM7 2h2v1h-2ZM7 3h2v1h-2ZM7 4h2v1h-2ZM7 5h2v1h-2ZM7 6h2v1h-2ZM4 7h8v1h-8ZM5 8h6v1h-6ZM6 9h4v1h-4ZM1 10h2v1h-2ZM7 10h2v1h-2ZM13 10h2v1h-2ZM1 11h2v1h-2ZM13 11h2v1h-2ZM1 12h2v1h-2ZM13 12h2v1h-2ZM1 13h14v1h-14ZM1 14h14v1h-14Z",
        // Small marks, each on the grid of the size it's drawn at (see `grid_of`), and drawn
        // pixel by pixel: at these sizes a slanted edge would leave grey half-pixels.
        Icon::ArrowUp => "M6 5h2v1h-2ZM5 6h4v1h-4ZM4 7h6v1h-6ZM3 8h8v1h-8Z",
        Icon::ArrowDown => "M3 5h8v1h-8ZM4 6h6v1h-6ZM5 7h4v1h-4ZM6 8h2v1h-2Z",
        Icon::Close => "M2 2h2v1h-2ZM8 2h2v1h-2ZM3 3h2v1h-2ZM7 3h2v1h-2ZM4 4h4v1h-4ZM5 5h2v1h-2ZM4 6h4v1h-4ZM3 7h2v1h-2ZM7 7h2v1h-2ZM2 8h2v1h-2ZM8 8h2v1h-2Z",
        Icon::Crumb => "M3 1h1v1h-1ZM3 2h2v1h-2ZM4 3h2v1h-2ZM5 4h2v1h-2ZM4 5h2v1h-2ZM3 6h2v1h-2ZM3 7h1v1h-1Z",
    }
}

/// The grid an icon is drawn on, in pixels. It's sharp drawn at that size or a whole multiple of
/// it: the small marks are drawn smaller than the rest, so each has a grid of its own.
pub fn grid_of(i: Icon) -> f32 {
    match i {
        Icon::Crumb => 10.0,
        Icon::Close => 12.0,
        Icon::ArrowUp | Icon::ArrowDown => 14.0,
        _ => 16.0,
    }
}

#[derive(Default)]
pub struct Icons {
    cache: HashMap<(Icon, i32), (Vec<u8>, i32)>,
}

impl Icons {
    pub fn draw(&mut self, canvas: &mut Canvas, icon: Icon, x: i32, y: i32, size: i32, color: u32) {
        let (mask, s) = self.cache.entry((icon, size)).or_insert_with(|| {
            let scale = size as f32 / grid_of(icon);
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
        for i in [Folder, FolderLink, File, FileLink, Image, Video, Archive, Code, Document, Back, Forward, Up, Home, Trash, Disk, ArrowUp, ArrowDown, Close, Download, Crumb] {
            let mut px = vec![0u32; 32 * 32];
            let mut c = Canvas::new(&mut px, 32, 32);
            Icons::default().draw(&mut c, i, 0, 0, 32, 0xffffff);
            assert!(px.iter().any(|&p| p != 0), "{i:?} drew nothing");
        }
    }

    /// Icons are sharp at the size they're drawn, and at twice it: every pixel is fully on or
    /// fully off, with nothing smoothed in between to blur them.
    #[test]
    fn icons_are_whole_pixels() {
        use Icon::*;
        for i in [Folder, FolderLink, File, FileLink, Image, Video, Archive, Code, Document, Back, Forward, Up, Home, Trash, Disk, Eject, Lock, Download, ArrowUp, ArrowDown, Close, Crumb] {
            for times in [1, 2] {
                let size = grid_of(i) as i32 * times;
                let mut px = vec![0u32; (size * size) as usize];
                let mut c = Canvas::new(&mut px, size, size);
                Icons::default().draw(&mut c, i, 0, 0, size, 0xffffff);
                let partial = px.iter().filter(|&&p| p != 0 && p & 0xffffff != 0xffffff).count();
                assert_eq!(partial, 0, "{i:?} at {size} pixels has {partial} half-covered pixels");
            }
        }
        // Music keeps its original, smoothed outline, which reads better than any pixel version.
    }
}
