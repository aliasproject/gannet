//! Text: shaping and rasterizing with swash, caching both, with font fallback.
//!
//! The primary font's mapped file is the only thing loaded before the first frame.
//! A character no loaded font has gets looked up on a helper thread, shows blank for
//! a moment, and appears on the next paint once the font is in.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use swash::scale::image::Content;
use swash::scale::{Render, ScaleContext, Source, StrikeWith};
use swash::shape::{Direction, ShapeContext};
use swash::text::{Codepoint, Script};
use swash::zeno::{Format, Vector};
use swash::{CacheKey, FontRef};

use crate::canvas::Canvas;

struct Face {
    data: &'static [u8],
    offset: u32,
    key: CacheKey,
    path: PathBuf,
    index: u32,
}

impl Face {
    fn font(&self) -> FontRef<'static> {
        FontRef {
            data: self.data,
            offset: self.offset,
            key: self.key,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Glyph {
    face: u8,
    id: u16,
    x: f32,
    y: f32,
}

/// A shaped line of text.
pub struct Run {
    glyphs: Vec<Glyph>,
    pub width: f32,
}

#[derive(PartialEq, Eq, Hash, Clone, Copy)]
struct GlyphKey {
    face: u8,
    id: u16,
    sub_x: u8,
}

#[derive(Clone, Copy)]
struct Cached {
    left: i32,
    top: i32,
    w: i32,
    h: i32,
    off: u32,
    color: bool,
}

const SHAPE_CACHE_MAX: usize = 4096;
const GLYPH_ARENA_MAX: usize = 8 * 1024 * 1024;

pub struct Text {
    faces: Vec<Face>,
    px: f32,
    /// Whole-pixel glyph placement with hinting, for the low-density case. High-density
    /// screens get unhinted glyphs at quarter-pixel positions.
    hinted: bool,
    pub ascent: f32,
    pub descent: f32,
    shape_ctx: ShapeContext,
    scale_ctx: ScaleContext,
    shaped: HashMap<String, Arc<Run>>,
    glyphs: HashMap<GlyphKey, Cached>,
    arena: Vec<u8>,
    /// Characters needing a fallback font, for the app to look up.
    missing: Vec<char>,
    tried: HashSet<char>,
    /// Text gamma, per mask coverage: light text on dark gets thinned a little.
    gamma: [u8; 256],
    /// Set when a fallback font arrived, so everything shaped so far is stale.
    pub generation: u64,
}

impl Text {
    pub fn new(px: f32, hinted: bool) -> Self {
        let mut t = Self {
            faces: Vec::new(),
            px,
            hinted,
            ascent: px * 0.8,
            descent: px * 0.2,
            shape_ctx: ShapeContext::new(),
            scale_ctx: ScaleContext::new(),
            shaped: HashMap::new(),
            glyphs: HashMap::new(),
            arena: Vec::new(),
            missing: Vec::new(),
            tried: HashSet::new(),
            gamma: [0; 256],
            generation: 0,
        };
        t.set_contrast(true);
        t
    }

    /// Adjusts coverage for how light-on-dark and dark-on-light text read differently.
    pub fn set_contrast(&mut self, light_on_dark: bool) {
        let exp = if light_on_dark { 1.15 } else { 0.85 };
        for (i, v) in self.gamma.iter_mut().enumerate() {
            *v = ((i as f32 / 255.0).powf(exp) * 255.0).round() as u8;
        }
        self.glyphs.clear();
        self.arena.clear();
    }

    /// Maps a font file. `false` if it can't be read or isn't a font.
    pub fn add_font(&mut self, path: &Path, index: u32) -> bool {
        if self.faces.len() >= u8::MAX as usize
            || self
                .faces
                .iter()
                .any(|f| f.path == path && f.index == index)
        {
            return false;
        }
        let Some(data) = map_file(path) else {
            return false;
        };
        let Some(font) = FontRef::from_index(data, index as usize) else {
            return false;
        };
        let first = self.faces.is_empty();
        self.faces.push(Face {
            data,
            offset: font.offset,
            key: font.key,
            path: path.to_path_buf(),
            index,
        });
        if first {
            self.update_metrics();
        }
        self.shaped.clear();
        self.generation += 1;
        true
    }

    /// Replaces the primary font, as when the system's choice changed.
    pub fn set_primary(&mut self, path: &Path, index: u32) -> bool {
        let Some(data) = map_file(path) else {
            return false;
        };
        let Some(font) = FontRef::from_index(data, index as usize) else {
            return false;
        };
        let face = Face {
            data,
            offset: font.offset,
            key: font.key,
            path: path.to_path_buf(),
            index,
        };
        if self.faces.is_empty() {
            self.faces.push(face);
        } else {
            self.faces[0] = face;
        }
        self.update_metrics();
        self.reset_caches();
        true
    }

    fn update_metrics(&mut self) {
        let m = self.faces[0].font().metrics(&[]).scale(self.px);
        self.ascent = m.ascent;
        self.descent = m.descent;
    }

    fn reset_caches(&mut self) {
        self.shaped.clear();
        self.glyphs.clear();
        self.arena.clear();
        self.generation += 1;
    }

    /// Changes the pixel size, as when the window's scale changes.
    pub fn set_size(&mut self, px: f32, hinted: bool) {
        if (px - self.px).abs() < f32::EPSILON && hinted == self.hinted {
            return;
        }
        self.px = px;
        self.hinted = hinted;
        if !self.faces.is_empty() {
            self.update_metrics();
        }
        self.reset_caches();
    }

    pub fn line_height(&self) -> f32 {
        self.ascent + self.descent
    }

    pub fn take_missing(&mut self) -> Vec<char> {
        std::mem::take(&mut self.missing)
    }

    /// Picks the first loaded face that has the character.
    fn face_for(&mut self, ch: char) -> u8 {
        for (i, f) in self.faces.iter().enumerate() {
            if f.font().charmap().map(ch) != 0 {
                return i as u8;
            }
        }
        // Plain spaces and controls aren't worth a font search.
        if !ch.is_control() && ch != ' ' && self.tried.insert(ch) {
            self.missing.push(ch);
        }
        0
    }

    /// Shapes a line, reusing the last result for the same text.
    pub fn shape(&mut self, s: &str) -> Arc<Run> {
        if let Some(r) = self.shaped.get(s) {
            return r.clone();
        }
        let run = Arc::new(self.shape_uncached(s));
        if self.shaped.len() >= SHAPE_CACHE_MAX {
            self.shaped.clear();
        }
        self.shaped.insert(s.to_string(), run.clone());
        run
    }

    fn shape_uncached(&mut self, s: &str) -> Run {
        let mut glyphs = Vec::with_capacity(s.len());
        let mut x = 0.0f32;
        if self.faces.is_empty() {
            return Run { glyphs, width: 0.0 };
        }
        // Split where the font or script changes. Marks and joiners stay with what's
        // before them, so accents and emoji sequences stay in one piece.
        let mut start = 0;
        let mut seg: Option<(u8, Script)> = None;
        let mut segments = Vec::new();
        for (i, ch) in s.char_indices() {
            let joins = ch.is_ascii_control() || joins_previous(ch);
            let script = ch.script();
            let generic = matches!(script, Script::Common | Script::Inherited | Script::Unknown);
            let face = match (&seg, joins) {
                (Some((f, _)), true) => *f,
                _ => self.face_for(ch),
            };
            match &mut seg {
                None => seg = Some((face, script)),
                Some((f, sc)) => {
                    let script_differs = !generic
                        && *sc != script
                        && !matches!(*sc, Script::Common | Script::Inherited | Script::Unknown);
                    if *f != face || script_differs {
                        segments.push((start, i, *f, *sc));
                        start = i;
                        seg = Some((face, script));
                    } else if !generic {
                        *sc = script;
                    }
                }
            }
        }
        if let Some((f, sc)) = seg {
            segments.push((start, s.len(), f, sc));
        }
        for (a, b, face, script) in segments {
            let font = self.faces[face as usize].font();
            let mut shaper = self
                .shape_ctx
                .builder(font)
                .script(script)
                .direction(Direction::LeftToRight)
                .size(self.px)
                .build();
            shaper.add_str(&s[a..b]);
            let hinted = self.hinted;
            shaper.shape_with(|c| {
                for g in c.glyphs {
                    glyphs.push(Glyph {
                        face,
                        id: g.id,
                        x: x + g.x,
                        y: g.y,
                    });
                    x += if hinted { g.advance.round() } else { g.advance };
                }
            });
        }
        Run { glyphs, width: x }
    }

    pub fn measure(&mut self, s: &str) -> f32 {
        self.shape(s).width
    }

    fn glyph(&mut self, key: GlyphKey) -> Option<Cached> {
        if let Some(c) = self.glyphs.get(&key) {
            return Some(*c);
        }
        if self.arena.len() > GLYPH_ARENA_MAX {
            self.glyphs.clear();
            self.arena.clear();
        }
        let font = self.faces.get(key.face as usize)?.font();
        let mut scaler = self
            .scale_ctx
            .builder(font)
            .size(self.px)
            .hint(self.hinted)
            .build();
        // No ColorOutline: it gives premultiplied pixels, and color bitmaps are what's
        // installed here. Such fonts fall back to their plain outlines.
        let image = Render::new(&[Source::ColorBitmap(StrikeWith::BestFit), Source::Outline])
            .format(Format::Alpha)
            .offset(Vector::new(key.sub_x as f32 / 4.0, 0.0))
            .render(&mut scaler, key.id)?;
        let off = self.arena.len() as u32;
        let color = matches!(image.content, Content::Color);
        if color {
            self.arena.extend_from_slice(&image.data);
        } else {
            let gamma = &self.gamma;
            self.arena
                .extend(image.data.iter().map(|&a| gamma[a as usize]));
        }
        let c = Cached {
            left: image.placement.left,
            top: image.placement.top,
            w: image.placement.width as i32,
            h: image.placement.height as i32,
            off,
            color,
        };
        self.glyphs.insert(key, c);
        Some(c)
    }

    /// Draws a run with its origin at `x`, baseline at `y`, cut at `max_x`.
    pub fn draw(&mut self, canvas: &mut Canvas, run: &Run, x: f32, y: i32, color: u32) {
        let clip = canvas.clip();
        for g in &run.glyphs {
            let gx = x + g.x;
            if gx > clip.right() as f32 {
                break;
            }
            let (ix, sub_x) = if self.hinted {
                (gx.round() as i32, 0)
            } else {
                let fl = gx.floor();
                (fl as i32, (((gx - fl) * 4.0) as u8).min(3))
            };
            let Some(c) = self.glyph(GlyphKey {
                face: g.face,
                id: g.id,
                sub_x,
            }) else {
                continue;
            };
            if c.w == 0 || c.h == 0 {
                continue;
            }
            let (dx, dy) = (ix + c.left, y - c.top - g.y.round() as i32);
            if c.color {
                let end = c.off as usize + (c.w * c.h * 4) as usize;
                canvas.rgba(&self.arena[c.off as usize..end], c.w, c.h, dx, dy);
            } else {
                let end = c.off as usize + (c.w * c.h) as usize;
                canvas.mask(&self.arena[c.off as usize..end], c.w, c.h, dx, dy, color);
            }
        }
    }

    pub fn draw_str(&mut self, canvas: &mut Canvas, s: &str, x: f32, y: i32, color: u32) {
        let run = self.shape(s);
        self.draw(canvas, &run, x, y, color);
    }

    /// Draws text that must fit in `max_w`, cutting from the middle so the end of a
    /// file name (usually the extension or a number) stays visible.
    pub fn draw_fitted(
        &mut self,
        canvas: &mut Canvas,
        s: &str,
        x: f32,
        y: i32,
        max_w: f32,
        color: u32,
    ) {
        let run = self.shape(s);
        if run.width <= max_w {
            self.draw(canvas, &run, x, y, color);
            return;
        }
        let fitted = self.fit(&run, max_w);
        self.draw(canvas, &fitted, x, y, color);
    }

    /// A copy of `run` with the middle replaced by an ellipsis, at most `max_w` wide.
    fn fit(&mut self, run: &Run, max_w: f32) -> Run {
        let ell = self.shape("…");
        // Too narrow even for the ellipsis: drawing nothing beats running into the next column.
        if max_w < ell.width {
            return Run {
                glyphs: Vec::new(),
                width: 0.0,
            };
        }
        let keep = (max_w - ell.width).max(0.0);
        let tail_w = (keep * 0.33).min(keep);
        let head_w = keep - tail_w;
        let mut glyphs = Vec::new();
        for g in &run.glyphs {
            if g.x >= head_w {
                break;
            }
            // The next glyph's start stands in for this one's right edge.
            glyphs.push(*g);
        }
        // Drop the last head glyph if it could spill past the limit.
        if let Some(last) = glyphs.len().checked_sub(1)
            && self.next_x(run, last) > head_w
        {
            glyphs.pop();
        }
        let head_end = glyphs
            .len()
            .checked_sub(1)
            .map_or(0.0, |i| self.next_x(run, i));
        for g in &ell.glyphs {
            glyphs.push(Glyph {
                x: g.x + head_end,
                ..*g
            });
        }
        let after_ell = head_end + ell.width;
        let tail_from = run.width - tail_w;
        let mut shift = None;
        for g in &run.glyphs {
            if g.x >= tail_from {
                let s = *shift.get_or_insert(after_ell - g.x);
                glyphs.push(Glyph { x: g.x + s, ..*g });
            }
        }
        Run {
            glyphs,
            width: max_w.min(run.width),
        }
    }

    fn next_x(&self, run: &Run, i: usize) -> f32 {
        run.glyphs.get(i + 1).map_or(run.width, |g| g.x)
    }

    /// Byte offset in `s` nearest to `x` from the start of the text, for clicks in a field.
    pub fn offset_at(&mut self, s: &str, x: f32) -> usize {
        let mut best = (0usize, f32::MAX);
        // Not before a mark or joiner: that would split a character from its accent.
        let mut ends: Vec<usize> = s
            .char_indices()
            .filter(|&(_, c)| !joins_previous(c))
            .map(|(i, _)| i)
            .collect();
        ends.push(s.len());
        for i in ends {
            let w = self.measure(&s[..i]);
            let d = (w - x).abs();
            if d <= best.1 {
                best = (i, d);
            }
        }
        best.0
    }
}

/// Characters that stay attached to the one before them.
fn joins_previous(c: char) -> bool {
    matches!(c, '\u{200d}' | '\u{fe0f}') || ('\u{300}'..='\u{36f}').contains(&c)
}

/// Maps a file for the life of the process. Fonts stay loaded until exit, and there are few.
fn map_file(path: &Path) -> Option<&'static [u8]> {
    use rustix::mm::{MapFlags, ProtFlags, mmap};
    let file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len() as usize;
    if len == 0 {
        return None;
    }
    // SAFETY: a fresh private read-only mapping, never unmapped, so the slice is valid
    // for 'static. Fonts being replaced in place is the one risk, the same as for every
    // program that maps fonts.
    unsafe {
        let p = mmap(
            std::ptr::null_mut(),
            len,
            ProtFlags::READ,
            MapFlags::PRIVATE,
            &file,
            0,
        )
        .ok()?;
        Some(std::slice::from_raw_parts(p as *const u8, len))
    }
}

/// Asks fontconfig for a font, through `fc-match` so startup doesn't pay to load it
/// as a library. `pattern` is like `sans` or `sans:charset=4e00`.
pub fn fc_match(pattern: &str) -> Option<(PathBuf, u32)> {
    let out = std::process::Command::new("fc-match")
        .args(["-f", "%{file}\t%{index}", pattern])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_fc_match(&String::from_utf8_lossy(&out.stdout))
}

fn parse_fc_match(text: &str) -> Option<(PathBuf, u32)> {
    let (file, index) = text.split_once('\t')?;
    if file.is_empty() {
        return None;
    }
    Some((PathBuf::from(file), index.trim().parse().unwrap_or(0)))
}

/// The font to use, from a small cache file so the first frame needn't start a process.
pub fn primary_font(cache_home: &Path) -> Option<(PathBuf, u32)> {
    let cache = cache_home.join("gannet/font");
    if let Ok(text) = std::fs::read_to_string(&cache)
        && let Some((path, index)) = parse_fc_match(&text)
        && path.is_file()
    {
        return Some((path, index));
    }
    let found = fc_match("sans")?;
    store_primary_font(cache_home, &found);
    Some(found)
}

pub fn store_primary_font(cache_home: &Path, found: &(PathBuf, u32)) {
    let dir = cache_home.join("gannet");
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(
        dir.join("font"),
        format!("{}\t{}", found.0.display(), found.1),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text() -> Option<Text> {
        let (path, index) = fc_match("sans")?;
        let mut t = Text::new(14.0, true);
        assert!(t.add_font(&path, index));
        Some(t)
    }

    #[test]
    fn with_no_fonts_nothing_panics() {
        let mut t = Text::new(14.0, true);
        assert_eq!(t.measure("hello"), 0.0);
        let mut px = vec![0u32; 100];
        let mut c = Canvas::new(&mut px, 10, 10);
        t.draw_str(&mut c, "hello", 0.0, 5, 1);
        t.draw_fitted(&mut c, "hello", 0.0, 5, 3.0, 1);
    }

    #[test]
    fn bad_font_files_are_refused() {
        let mut t = Text::new(14.0, true);
        assert!(!t.add_font(Path::new("/nonexistent/gannet.ttf"), 0));
        assert!(!t.add_font(Path::new("/proc/self/cmdline"), 0));
    }

    #[test]
    fn longer_text_is_wider_and_drawing_marks_pixels() {
        let Some(mut t) = text() else { return };
        assert!(t.measure("hello world") > t.measure("hello"));
        let mut px = vec![0u32; 100 * 30];
        let mut c = Canvas::new(&mut px, 100, 30);
        t.draw_str(&mut c, "hello", 2.0, 20, 0xffffff);
        assert!(px.iter().any(|&p| p != 0));
    }

    #[test]
    fn fitted_text_stays_in_bounds_and_keeps_the_end() {
        let Some(mut t) = text() else { return };
        let long = format!("{}-ending.pdf", "a".repeat(200));
        let run = t.shape(&long);
        let fitted = t.fit(&run, 120.0);
        assert!(fitted.width <= 120.0);
        let last = fitted.glyphs.last().unwrap();
        assert!(last.x < 120.0, "last glyph at {}", last.x);
        // Tail glyphs came through: the final glyph is the final glyph of the original.
        assert_eq!(last.id, run.glyphs.last().unwrap().id);
    }

    #[test]
    fn fit_copes_with_widths_too_small_for_anything() {
        let Some(mut t) = text() else { return };
        let run = t.shape("something long enough");
        for w in [0.0, 1.0] {
            let f = t.fit(&run, w);
            assert!(f.glyphs.is_empty(), "nothing fits in {w}");
        }
        let _ = t.fit(&run, 8.0);
    }

    #[test]
    fn empty_and_odd_strings_shape() {
        let Some(mut t) = text() else { return };
        assert_eq!(t.measure(""), 0.0);
        for s in [
            "\u{200d}",
            "e\u{301}",
            "\u{1F600}",
            "a\u{0}b",
            "\u{FFFD}\u{FFFD}",
            "日本語 mixed",
        ] {
            t.measure(s);
        }
    }

    #[test]
    fn characters_with_no_font_are_requested_once() {
        let Some(mut t) = text() else { return };
        // A private-use character no system font maps to.
        t.measure("\u{10FFFD}");
        t.measure("\u{10FFFD}x");
        assert_eq!(t.take_missing(), vec!['\u{10FFFD}']);
    }

    #[test]
    fn offset_at_finds_the_nearest_boundary() {
        let Some(mut t) = text() else { return };
        assert_eq!(t.offset_at("héllo", -5.0), 0);
        assert_eq!(t.offset_at("héllo", 10_000.0), "héllo".len());
        let x = t.measure("hé");
        assert_eq!(t.offset_at("héllo", x), "hé".len());
    }

    #[test]
    fn a_click_past_an_accent_lands_after_it() {
        let Some(mut t) = text() else { return };
        let s = "cafe\u{301}";
        assert_eq!(t.offset_at(s, 10_000.0), s.len());
        let x = t.measure("cafe");
        assert_ne!(
            t.offset_at(s, x),
            "cafe".len(),
            "never between a letter and its accent"
        );
    }

    #[test]
    fn a_second_face_of_a_collection_is_accepted() {
        let path = Path::new("/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc");
        if !path.exists() {
            return;
        }
        let mut t = Text::new(14.0, true);
        assert!(t.add_font(path, 0));
        assert!(!t.add_font(path, 0), "the same face twice is pointless");
        assert!(t.add_font(path, 1));
    }

    #[test]
    fn fc_match_output_parses() {
        assert_eq!(parse_fc_match("/a b.ttf\t2"), Some(("/a b.ttf".into(), 2)));
        assert_eq!(parse_fc_match("\t0"), None);
        assert_eq!(parse_fc_match("nonsense"), None);
    }
}
