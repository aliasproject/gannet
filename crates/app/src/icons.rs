//! Icons, drawn from small vector paths and tinted from the theme. Being masks, they
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
        Icon::Audio => "M5 4 14 1.5v2.5L7 6v6.5H5V4ZM12 3h2v8.5h-2ZM2 12.5a2.5 2.5 0 0 1 5 0a2.5 2.5 0 0 1-5 0ZM9 11.5a2.5 2.5 0 0 1 5 0a2.5 2.5 0 0 1-5 0Z",
        Icon::Archive => "M1 2h14v1h-14ZM1 3h14v1h-14ZM1 4h14v1h-14ZM2 6h12v1h-12ZM2 7h12v1h-12ZM2 8h4v1h-4ZM10 8h4v1h-4ZM2 9h12v1h-12ZM2 10h12v1h-12ZM2 11h12v1h-12ZM2 12h12v1h-12ZM2 13h12v1h-12Z",
        Icon::Code => "M3 1h6v1h-6ZM10 1h1v1h-1ZM3 2h6v1h-6ZM10 2h2v1h-2ZM3 3h6v1h-6ZM10 3h3v1h-3ZM3 4h6v1h-6ZM3 5h10v1h-10ZM3 6h10v1h-10ZM3 7h10v1h-10ZM3 8h3v1h-3ZM7 8h2v1h-2ZM10 8h3v1h-3ZM3 9h2v1h-2ZM6 9h4v1h-4ZM11 9h2v1h-2ZM3 10h1v1h-1ZM5 10h6v1h-6ZM12 10h1v1h-1ZM3 11h2v1h-2ZM6 11h4v1h-4ZM11 11h2v1h-2ZM3 12h3v1h-3ZM7 12h2v1h-2ZM10 12h3v1h-3ZM3 13h10v1h-10ZM3 14h10v1h-10Z",
        Icon::Document => "M3 1h6v1h-6ZM10 1h1v1h-1ZM3 2h6v1h-6ZM10 2h2v1h-2ZM3 3h6v1h-6ZM10 3h3v1h-3ZM3 4h6v1h-6ZM3 5h10v1h-10ZM3 6h10v1h-10ZM3 7h2v1h-2ZM11 7h2v1h-2ZM3 8h10v1h-10ZM3 9h2v1h-2ZM11 9h2v1h-2ZM3 10h10v1h-10ZM3 11h2v1h-2ZM9 11h4v1h-4ZM3 12h10v1h-10ZM3 13h10v1h-10ZM3 14h10v1h-10Z",
        // Lucide's chevron-left.
        Icon::Back => "M15 18l-6-6 6-6",
        // Lucide's chevron-right.
        Icon::Forward => "M9 18l6-6-6-6",
        // Lucide's chevron-up.
        Icon::Up => "M18 15l-6-6-6 6",
        Icon::Home => "M7 1h2v1h-2ZM6 2h4v1h-4ZM5 3h6v1h-6ZM4 4h8v1h-8ZM3 5h10v1h-10ZM2 6h12v1h-12ZM1 7h14v1h-14ZM3 8h10v1h-10ZM3 9h10v1h-10ZM3 10h3v1h-3ZM10 10h3v1h-3ZM3 11h3v1h-3ZM10 11h3v1h-3ZM3 12h3v1h-3ZM10 12h3v1h-3ZM3 13h3v1h-3ZM10 13h3v1h-3ZM3 14h3v1h-3ZM10 14h3v1h-3Z",
        Icon::Trash => "M6 1h4v1h-4ZM2 2h12v1h-12ZM2 3h12v1h-12ZM4 5h8v1h-8ZM4 6h8v1h-8ZM4 7h2v1h-2ZM7 7h2v1h-2ZM10 7h2v1h-2ZM4 8h2v1h-2ZM7 8h2v1h-2ZM10 8h2v1h-2ZM4 9h2v1h-2ZM7 9h2v1h-2ZM10 9h2v1h-2ZM4 10h2v1h-2ZM7 10h2v1h-2ZM10 10h2v1h-2ZM4 11h2v1h-2ZM7 11h2v1h-2ZM10 11h2v1h-2ZM4 12h2v1h-2ZM7 12h2v1h-2ZM10 12h2v1h-2ZM4 13h8v1h-8ZM4 14h8v1h-8Z",
        // Lucide's hard-drive.
        Icon::Disk => "M22 12L2 12M5.45 5.11 2 12v6a2 2 0 0 0 2 2h16a2 2 0 0 0 2-2v-6l-3.45-6.89A2 2 0 0 0 16.76 4H7.24a2 2 0 0 0-1.79 1.11zM6 16L6.01 16M10 16L10.01 16",
        Icon::Eject => "M7 3h2v1h-2ZM6 4h4v1h-4ZM5 5h6v1h-6ZM4 6h8v1h-8ZM3 7h10v1h-10ZM2 8h12v1h-12ZM2 10h12v1h-12ZM2 11h12v1h-12Z",
        // Lucide's lock.
        Icon::Lock => "M5 11h14a2 2 0 0 1 2 2v7a2 2 0 0 1 -2 2h-14a2 2 0 0 1 -2 -2v-7a2 2 0 0 1 2 -2ZM7 11V7a5 5 0 0 1 10 0v4",
        // An arrow down into a tray.
        Icon::Download => "M7 1h2v1h-2ZM7 2h2v1h-2ZM7 3h2v1h-2ZM7 4h2v1h-2ZM7 5h2v1h-2ZM7 6h2v1h-2ZM4 7h8v1h-8ZM5 8h6v1h-6ZM6 9h4v1h-4ZM1 10h2v1h-2ZM7 10h2v1h-2ZM13 10h2v1h-2ZM1 11h2v1h-2ZM13 11h2v1h-2ZM1 12h2v1h-2ZM13 12h2v1h-2ZM1 13h14v1h-14ZM1 14h14v1h-14Z",
        // Small marks, each on the grid of the size it's drawn at (see `grid_of`), and drawn
        // Lucide's chevron-up.
        Icon::ArrowUp => "M18 15l-6-6-6 6",
        // Lucide's chevron-down.
        Icon::ArrowDown => "M6 9l6 6 6-6",
        Icon::Close => "M2 2h2v1h-2ZM8 2h2v1h-2ZM3 3h2v1h-2ZM7 3h2v1h-2ZM4 4h4v1h-4ZM5 5h2v1h-2ZM4 6h4v1h-4ZM3 7h2v1h-2ZM7 7h2v1h-2ZM2 8h2v1h-2ZM8 8h2v1h-2Z",
        // Lucide's chevron-right.
        Icon::Crumb => "M9 18l6-6-6-6",
    }
}

/// The chevrons, the drive and the lock are Lucide's (https://lucide.dev, ISC; see LICENSES/lucide.txt): outlines on a
/// 24-unit grid, stroked 2 units wide with round ends, as Lucide draws them. The rest are solid.
fn stroked(i: Icon) -> bool {
    matches!(i, Icon::Back | Icon::Forward | Icon::Up | Icon::Crumb | Icon::ArrowUp | Icon::ArrowDown | Icon::Disk | Icon::Lock)
}
/// Lucide's grid and stroke, in its units.
const LUCIDE_GRID: f32 = 24.0;
const LUCIDE_STROKE: f32 = 2.0;
/// The thinnest a chevron's stroke gets, in pixels, so those drawn at 10 or 14 pixels don't fade.
const MIN_STROKE_PX: f32 = 1.25;

/// The grid an icon is drawn on, in pixels. It's sharp drawn at that size or a whole multiple of
/// it: the close cross is drawn smaller than the rest, so it has a grid of its own.
pub fn grid_of(i: Icon) -> f32 {
    match i {
        _ if stroked(i) => LUCIDE_GRID,
        Icon::Close => 12.0,
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
            let mut mask = Mask::new(path_of(icon));
            let mut stroke = Stroke::new(LUCIDE_STROKE.max(MIN_STROKE_PX / scale));
            stroke.cap(Cap::Round).join(Join::Round);
            if stroked(icon) {
                mask.style(stroke);
            }
            let (data, _) = mask.size(size as u32, size as u32).format(Format::Alpha).transform(Some(Transform::scale(scale, scale))).render();
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
        for i in [Folder, FolderLink, File, FileLink, Image, Video, Archive, Code, Document, Home, Trash, Eject, Download, Close] {
            for times in [1, 2] {
                let size = grid_of(i) as i32 * times;
                let mut px = vec![0u32; (size * size) as usize];
                let mut c = Canvas::new(&mut px, size, size);
                Icons::default().draw(&mut c, i, 0, 0, size, 0xffffff);
                let partial = px.iter().filter(|&&p| p != 0 && p & 0xffffff != 0xffffff).count();
                assert_eq!(partial, 0, "{i:?} at {size} pixels has {partial} half-covered pixels");
            }
        }
        // Music's stems are whole pixels under a smooth slanted beam, and its round heads stand two
        // pixels apart.
        let mut px = vec![0u32; 16 * 16];
        let mut c = Canvas::new(&mut px, 16, 16);
        Icons::default().draw(&mut c, Audio, 0, 0, 16, 0xffffff);
        let at = |x: usize, y: usize| px[y * 16 + x];
        assert!(at(10, 3) != 0 && at(5, 8) == 0xffffff && at(12, 7) == 0xffffff && at(3, 12) == 0xffffff && at(11, 11) == 0xffffff, "beam, stems and both heads");
        assert!((10..16).all(|y| at(7, y) == 0 && at(8, y) == 0), "the heads stand apart");
    }
}
