//! Showing a document (see `doc`) in the preview: the fonts it's set in, laying its words out in
//! lines to the window's width, and drawing the lines that are in view.
//!
//! A plain text file is set in a monospaced font and wrapped by counting columns, so even a long
//! one lays out without measuring a word. Markdown is measured word by word.

use std::path::{Path, PathBuf};

use crate::canvas::{Canvas, Rect};
use crate::doc::{self, Block, Doc, Kind};
use crate::text::{self, Text};

const BODY_PX: f32 = 15.0;
const MONO_PX: f32 = 13.5;
const HEADING_PX: [f32; 3] = [24.0, 20.0, 17.0];
/// Spaces a tab stands for.
const TAB: usize = 4;

/// Which of the document's fonts something is set in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Face {
    Body,
    Bold,
    Italic,
    Mono,
    Heading(u8),
}

/// The fonts fontconfig is asked for, beyond the one the rest of the app uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Mono,
    Bold,
    Italic,
}

impl Role {
    pub const ALL: [Role; 3] = [Role::Mono, Role::Bold, Role::Italic];

    pub fn pattern(self) -> &'static str {
        match self {
            Role::Mono => "monospace",
            Role::Bold => "sans:bold",
            Role::Italic => "sans:italic",
        }
    }

    /// The cache file that remembers what fontconfig said, so the next document doesn't wait.
    fn cache_name(self) -> &'static str {
        match self {
            Role::Mono => "font-mono",
            Role::Bold => "font-bold",
            Role::Italic => "font-italic",
        }
    }
}

pub struct Fonts {
    body: Text,
    bold: Text,
    italic: Text,
    mono: Text,
    headings: [Text; 3],
    scale: f64,
}

impl Fonts {
    /// Starts from the app's own fonts, and what fontconfig chose last time for the rest.
    pub fn new(ui: &Text, scale: f64, light_on_dark: bool, cache_home: Option<&Path>) -> Self {
        let fonts = ui.fonts();
        let make = |px: f32, role: Option<Role>| {
            let mut t = Text::new(px * scale as f32, scale < 1.5);
            t.set_contrast(light_on_dark);
            for (path, index) in &fonts {
                t.add_font(path, *index);
            }
            if let Some((path, index)) = role.zip(cache_home).and_then(|(role, home)| text::cached_font(home, role.cache_name())) {
                t.set_primary(&path, index);
            }
            t
        };
        Self {
            body: make(BODY_PX, None),
            bold: make(BODY_PX, Some(Role::Bold)),
            italic: make(BODY_PX, Some(Role::Italic)),
            mono: make(MONO_PX, Some(Role::Mono)),
            headings: HEADING_PX.map(|px| make(px, Some(Role::Bold))),
            scale,
        }
    }

    fn all(&mut self) -> impl Iterator<Item = &mut Text> {
        [&mut self.body, &mut self.bold, &mut self.italic, &mut self.mono].into_iter().chain(self.headings.iter_mut())
    }

    pub fn get(&mut self, face: Face) -> &mut Text {
        match face {
            Face::Body => &mut self.body,
            Face::Bold => &mut self.bold,
            Face::Italic => &mut self.italic,
            Face::Mono => &mut self.mono,
            Face::Heading(i) => &mut self.headings[(i as usize).min(2)],
        }
    }

    /// Changes over to what fontconfig says now, and remembers it for next time.
    pub fn set_role(&mut self, role: Role, path: &Path, index: u32) -> bool {
        let mut changed = false;
        let texts: Vec<&mut Text> = match role {
            Role::Mono => vec![&mut self.mono],
            Role::Italic => vec![&mut self.italic],
            Role::Bold => std::iter::once(&mut self.bold).chain(self.headings.iter_mut()).collect(),
        };
        for t in texts {
            changed |= t.set_primary(path, index);
        }
        changed
    }

    /// A font for characters none of the others have.
    /// Returns whether any of them didn't have it already.
    pub fn add_fallback(&mut self, path: &Path, index: u32) -> bool {
        self.all().fold(false, |added, t| t.add_font(path, index) | added)
    }

    pub fn take_missing(&mut self) -> Vec<char> {
        let mut missing: Vec<char> = self.all().flat_map(|t| t.take_missing()).collect();
        missing.sort_unstable();
        missing.dedup();
        missing
    }

    pub fn set_scale(&mut self, scale: f64) {
        self.scale = scale;
        let sizes = [BODY_PX, BODY_PX, BODY_PX, MONO_PX].into_iter().chain(HEADING_PX);
        let list: Vec<&mut Text> = self.all().collect();
        for (t, px) in list.into_iter().zip(sizes) {
            t.set_size(px * scale as f32, scale < 1.5);
        }
    }

    pub fn set_contrast(&mut self, light_on_dark: bool) {
        for t in self.all() {
            t.set_contrast(light_on_dark);
        }
    }

    /// Changes whenever anything already laid out might now measure differently.
    fn generation(&mut self) -> u64 {
        self.all().map(|t| t.generation).sum()
    }

    fn px(&self, v: f64) -> i32 {
        (v * self.scale).round() as i32
    }
}

/// Looks up the document fonts with fontconfig, off the UI thread, keeping what it finds for the
/// next time. `send` gets each one that's changed.
pub fn find_fonts(cache_home: Option<PathBuf>, send: impl Fn(Role, PathBuf, u32) + Send + 'static) {
    let _ = std::thread::Builder::new().name("gannet-fonts".into()).spawn(move || {
        for role in Role::ALL {
            let Some(found) = text::fc_match(role.pattern()) else { continue };
            let cached = cache_home.as_deref().and_then(|home| text::cached_font(home, role.cache_name()));
            if cached.as_ref() != Some(&found) {
                if let Some(home) = &cache_home {
                    text::store_font(home, role.cache_name(), &found);
                }
                send(role, found.0, found.1);
            }
        }
    });
}

/// A run of words drawn in one go.
#[derive(Debug, PartialEq)]
pub struct Piece {
    pub x: f32,
    pub face: Face,
    pub style: u8,
    pub text: String,
}

#[derive(Debug, Default, PartialEq)]
pub enum Mark {
    #[default]
    None,
    /// A thin line across: a Markdown rule.
    Rule,
    /// Part of a code block, whose background runs between these.
    Code(i32, i32),
}

#[derive(Debug, PartialEq)]
pub struct Line {
    pub y: i32,
    pub h: i32,
    /// From `y`.
    pub baseline: i32,
    pub quote: u8,
    pub mark: Mark,
    /// A list item's bullet or number, and where it goes.
    pub marker: Option<(f32, String)>,
    pub pieces: Vec<Piece>,
}

/// Where everything goes, for one width.
pub struct Layout {
    pub width: i32,
    generation: u64,
    pub height: i32,
    pub lines: Vec<Line>,
}

impl Layout {
    /// Whether it still fits a window this wide, set in these fonts.
    pub fn fits(&self, width: i32, fonts: &mut Fonts) -> bool {
        self.width == width && self.generation == fonts.generation()
    }

    /// The first line that reaches below `y`.
    pub fn line_at(&self, y: i32) -> usize {
        self.lines.partition_point(|l| l.y + l.h <= y)
    }

    /// How far a line of body text scrolls by.
    pub fn line_height(fonts: &mut Fonts) -> f64 {
        fonts.body.line_height() as f64
    }
}

/// How many columns a character takes in a monospaced font: two for the wide characters of East
/// Asian scripts and emoji, none for marks that sit on the one before.
fn columns(c: char) -> usize {
    let u = c as u32;
    if ('\u{300}'..='\u{36f}').contains(&c) || matches!(u, 0x200b..=0x200f | 0xfe00..=0xfe0f) {
        0
    } else if matches!(u, 0x1100..=0x115f | 0x2e80..=0x303e | 0x3041..=0x33ff | 0x3400..=0x4dbf | 0x4e00..=0x9fff | 0xa000..=0xa4cf | 0xac00..=0xd7a3 | 0xf900..=0xfaff | 0xfe30..=0xfe4f | 0xff00..=0xff60 | 0xffe0..=0xffe6 | 0x1f300..=0x1f64f | 0x1f900..=0x1f9ff | 0x20000..=0x3fffd) {
        2
    } else {
        1
    }
}

/// Splits a line of text into pieces no wider than `fit` columns, with tabs as spaces.
fn wrap_columns(line: &str, fit: usize) -> Vec<String> {
    let fit = fit.max(1);
    let mut out = vec![String::new()];
    let mut used = 0;
    for c in line.chars() {
        let (c, n) = if c == '\t' { (' ', TAB - used % TAB) } else { (c, 1) };
        for _ in 0..n {
            let w = columns(c);
            if used + w > fit && used > 0 {
                out.push(String::new());
                used = 0;
            }
            if let Some(s) = out.last_mut() {
                s.push(c);
            }
            used += w;
        }
    }
    out
}

/// Lays `doc` out for a window `width` pixels wide.
pub fn lay_out(doc: &Doc, width: i32, fonts: &mut Fonts) -> Layout {
    let pad = fonts.px(20.0);
    let indent_w = fonts.px(22.0);
    let quote_w = fonts.px(16.0);
    let gap = fonts.body.line_height() as i32;
    let mut out = Layout { width, generation: fonts.generation(), height: 0, lines: Vec::new() };
    let mut y = fonts.px(16.0);
    for (i, block) in doc.blocks.iter().enumerate() {
        let left = pad + block.indent as i32 * indent_w + block.quote as i32 * quote_w;
        let right = (width - pad).max(left + 1);
        // Space above, by what the block is and what came before.
        let before = match (&block.kind, i) {
            (_, 0) => 0,
            (Kind::Heading(_), _) => gap,
            (Kind::Item(_), _) if matches!(doc.blocks[i - 1].kind, Kind::Item(_)) => gap / 5,
            _ => gap * 3 / 5,
        };
        y += before;
        match &block.kind {
            Kind::Plain | Kind::Code => {
                let code = block.kind == Kind::Code;
                let inset = if code { fonts.px(10.0) } else { 0 };
                let mono = &mut fonts.mono;
                let advance = mono.measure("0").max(1.0);
                let fit = ((right - left - inset * 2) as f32 / advance).floor().max(1.0) as usize;
                let (h, ascent) = (mono.line_height().ceil() as i32, mono.ascent.round() as i32);
                let text: String = block.spans.iter().map(|s| s.text.as_str()).collect();
                if code {
                    y += inset / 2;
                }
                let first = out.lines.len();
                for source in text.strip_suffix('\n').unwrap_or(&text).split('\n') {
                    for piece in wrap_columns(source, fit) {
                        let mark = if code { Mark::Code(left, right) } else { Mark::None };
                        out.lines.push(Line { y, h, baseline: ascent, quote: block.quote, mark, marker: None, pieces: vec![Piece { x: (left + inset) as f32, face: Face::Mono, style: doc::CODE, text: piece }] });
                        y += h;
                    }
                }
                // The box reaches a little above and below its text.
                if code && out.lines.len() > first {
                    out.lines[first].y -= inset / 2;
                    out.lines[first].h += inset / 2;
                    out.lines[first].baseline += inset / 2;
                    if let Some(last) = out.lines.last_mut() {
                        last.h += inset / 2;
                    }
                    y += inset / 2;
                }
            }
            Kind::Rule => {
                let h = gap;
                out.lines.push(Line { y, h, baseline: 0, quote: block.quote, mark: Mark::Rule, marker: None, pieces: Vec::new() });
                y += h;
            }
            Kind::Paragraph | Kind::Heading(_) | Kind::Item(_) => {
                y = flow(block, left, right, y, fonts, &mut out.lines);
            }
        }
    }
    out.height = y + fonts.px(24.0);
    out
}

/// The face for words in a style, in a block of this kind.
fn face_for(kind: &Kind, style: u8) -> Face {
    if style & doc::CODE != 0 {
        return Face::Mono;
    }
    match kind {
        Kind::Heading(level @ 1..=3) => Face::Heading(level - 1),
        Kind::Heading(_) => Face::Bold,
        _ if style & doc::BOLD != 0 => Face::Bold,
        _ if style & doc::ITALIC != 0 => Face::Italic,
        _ => Face::Body,
    }
}

/// Words, each with the space after it, or a line break on its own.
fn words(text: &str) -> impl Iterator<Item = &str> {
    let mut rest = text;
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        if let Some(after) = rest.strip_prefix('\n') {
            rest = after;
            return Some("\n");
        }
        let word_end = rest.find([' ', '\n']).unwrap_or(rest.len());
        let space_end = word_end + rest[word_end..].bytes().take_while(|&b| b == b' ').count();
        let (word, after) = rest.split_at(space_end);
        rest = after;
        Some(word)
    })
}

/// Lays out a block of running words from `y`, returning where the next block starts.
fn flow(block: &Block, left: i32, right: i32, mut y: i32, fonts: &mut Fonts, lines: &mut Vec<Line>) -> i32 {
    let mut line: Vec<Piece> = Vec::new();
    let mut x = left as f32;
    let marker = match &block.kind {
        Kind::Item(marker) => {
            let face = fonts.get(Face::Body);
            let w = face.measure(marker);
            Some((left as f32 - w - fonts.px(8.0) as f32, marker.clone()))
        }
        _ => None,
    };
    let mut marker = marker;
    let finish = |line: &mut Vec<Piece>, y: &mut i32, fonts: &mut Fonts, marker: &mut Option<(f32, String)>, lines: &mut Vec<Line>| {
        let faces: Vec<Face> = if line.is_empty() { vec![face_for(&block.kind, 0)] } else { line.iter().map(|p| p.face).collect() };
        let h = faces.iter().map(|&f| fonts.get(f).line_height()).fold(0.0, f32::max).ceil() as i32;
        let ascent = faces.iter().map(|&f| fonts.get(f).ascent).fold(0.0, f32::max).round() as i32;
        lines.push(Line { y: *y, h, baseline: ascent, quote: block.quote, mark: Mark::None, marker: marker.take(), pieces: std::mem::take(line) });
        *y += h;
    };
    for span in &block.spans {
        let face = face_for(&block.kind, span.style);
        for word in words(&span.text) {
            if word == "\n" {
                finish(&mut line, &mut y, fonts, &mut marker, lines);
                x = left as f32;
                continue;
            }
            let t = fonts.get(face);
            let w = t.measure(word);
            let ink = t.measure(word.trim_end_matches(' '));
            if x + ink > right as f32 && x > left as f32 {
                finish(&mut line, &mut y, fonts, &mut marker, lines);
                x = left as f32;
            }
            // A word longer than the whole line is broken wherever it has to be. Characters are
            // measured one at a time, as measuring each longer start of a long word would take
            // as long as the square of its length.
            if ink > (right - left) as f32 {
                let (mut part, mut part_w) = (String::new(), 0.0);
                let mut buf = [0u8; 4];
                for c in word.chars() {
                    let cw = fonts.get(face).measure(c.encode_utf8(&mut buf));
                    if x + part_w + cw > right as f32 && !part.is_empty() {
                        line.push(Piece { x, face, style: span.style, text: std::mem::take(&mut part) });
                        finish(&mut line, &mut y, fonts, &mut marker, lines);
                        x = left as f32;
                        part_w = 0.0;
                    }
                    part.push(c);
                    part_w += cw;
                }
                line.push(Piece { x, face, style: span.style, text: part });
                x += part_w;
                continue;
            }
            // Joined to the piece before when it looks the same, so a line is drawn in a few goes.
            match line.last_mut() {
                Some(last) if last.face == face && last.style == span.style => last.text.push_str(word),
                _ => line.push(Piece { x, face, style: span.style, text: word.to_string() }),
            }
            x += w;
        }
    }
    // An item with nothing in it still shows its bullet.
    if !line.is_empty() || marker.is_some() {
        finish(&mut line, &mut y, fonts, &mut marker, lines);
    }
    y
}

/// What the document is drawn with.
#[derive(Clone, Copy)]
pub struct Ink {
    pub text: u32,
    pub muted: u32,
    pub link: u32,
    pub code: u32,
    pub rule: u32,
}

/// Draws the lines in view, `scroll` pixels down, inside `area`.
pub fn paint(c: &mut Canvas, area: Rect, scroll: i32, layout: &Layout, fonts: &mut Fonts, ink: Ink) {
    let bar = fonts.px(3.0).max(1);
    let pad = fonts.px(20.0);
    let quote_w = fonts.px(16.0);
    let top = area.y - scroll;
    for line in &layout.lines[layout.line_at(scroll)..] {
        let y = top + line.y;
        if y >= area.bottom() {
            break;
        }
        match line.mark {
            Mark::Rule => c.fill(Rect::new(area.x + pad, y + line.h / 2, area.w - pad * 2, fonts.px(1.0).max(1)), ink.rule),
            Mark::Code(l, r) => c.fill(Rect::new(area.x + l, y, r - l, line.h), ink.code),
            Mark::None => {}
        }
        for q in 0..line.quote as i32 {
            c.fill(Rect::new(area.x + pad + q * quote_w, y, bar, line.h), ink.rule);
        }
        let baseline = y + line.baseline;
        if let Some((x, marker)) = &line.marker {
            fonts.get(Face::Body).draw_str(c, marker, area.x as f32 + x, baseline, ink.muted);
        }
        for piece in &line.pieces {
            let color = if piece.style & doc::LINK != 0 { ink.link } else if line.quote > 0 { ink.muted } else { ink.text };
            let x = area.x as f32 + piece.x;
            let t = fonts.get(piece.face);
            // Code among words gets a little box of its own.
            if piece.style & doc::CODE != 0 && line.mark == Mark::None && piece.face == Face::Mono && line.pieces.len() > 1 {
                let w = t.measure(piece.text.trim_end_matches(' ')).ceil() as i32;
                let h = t.line_height().ceil() as i32;
                c.rrect(Rect::new(x as i32 - 3, baseline - t.ascent.round() as i32 - 1, w + 6, h + 2), 4, ink.code);
            }
            t.draw_str(c, &piece.text, x, baseline, color);
            if piece.style & (doc::STRUCK | doc::LINK) != 0 {
                let w = t.measure(piece.text.trim_end_matches(' ')).ceil() as i32;
                let at = if piece.style & doc::STRUCK != 0 { baseline - (t.ascent * 0.3).round() as i32 } else { baseline + 2 };
                c.fill(Rect::new(x as i32, at, w, 1), color);
            }
        }
    }
    // Where in the document this is, when there's more of it than fits.
    if layout.height > area.h && area.h > 0 {
        let track = area.h - fonts.px(8.0);
        let thumb = ((area.h as f64 / layout.height as f64) * track as f64).max(fonts.px(24.0) as f64) as i32;
        let along = scroll as f64 / (layout.height - area.h).max(1) as f64;
        let y = area.y + fonts.px(4.0) + ((track - thumb) as f64 * along.clamp(0.0, 1.0)) as i32;
        let w = fonts.px(4.0).max(2);
        c.rrect(Rect::new(area.right() - w - fonts.px(3.0), y, w, thumb), w / 2, ink.rule);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_wraps_by_columns_with_wide_characters_and_tabs() {
        assert_eq!(wrap_columns("abcdefg", 3), ["abc", "def", "g"]);
        assert_eq!(wrap_columns("", 3), [""], "an empty line is still a line");
        assert_eq!(wrap_columns("a\tb", 8), ["a   b"], "a tab goes to the next stop");
        assert_eq!(wrap_columns("日本語", 4), ["日本", "語"], "two columns each");
        assert_eq!(wrap_columns("e\u{301}x", 2), ["e\u{301}x"], "a mark takes no room of its own");
        assert_eq!(wrap_columns("ab", 0), ["a", "b"], "a window too narrow still shows something");
    }

    fn fonts() -> Option<Fonts> {
        let (path, index) = text::fc_match("sans")?;
        let mut ui = Text::new(14.0, true);
        ui.add_font(&path, index);
        Some(Fonts::new(&ui, 1.0, false, None))
    }

    #[test]
    fn a_word_longer_than_the_line_is_broken_to_fit() {
        let Some(mut fonts) = fonts() else { return };
        let doc = doc::markdown(&format!("{}\n", "a".repeat(5000)), false);
        let width = 400;
        let layout = lay_out(&doc, width, &mut fonts);
        assert!(layout.lines.len() > 5, "broken over lines");
        let text: String = layout.lines.iter().flat_map(|l| l.pieces.iter().map(|p| p.text.as_str())).collect();
        assert_eq!(text.len(), 5000, "nothing lost");
        for line in &layout.lines {
            for p in &line.pieces {
                assert!(p.x + fonts.get(p.face).measure(&p.text) <= (width - fonts.px(20.0)) as f32 + 1.0, "inside the line");
            }
        }
    }

    #[test]
    fn words_keep_their_spaces_and_breaks() {
        let got: Vec<&str> = words("one two  three\nfour").collect();
        assert_eq!(got, ["one ", "two  ", "three", "\n", "four"]);
        assert_eq!(words("").count(), 0);
    }

    #[test]
    fn faces_follow_the_kind_and_the_style() {
        assert_eq!(face_for(&Kind::Heading(1), 0), Face::Heading(0));
        assert_eq!(face_for(&Kind::Heading(5), doc::ITALIC), Face::Bold);
        assert_eq!(face_for(&Kind::Heading(2), doc::CODE), Face::Mono);
        assert_eq!(face_for(&Kind::Paragraph, doc::BOLD | doc::ITALIC), Face::Bold);
        assert_eq!(face_for(&Kind::Paragraph, doc::ITALIC | doc::LINK), Face::Italic);
        assert_eq!(face_for(&Kind::Item("•".into()), 0), Face::Body);
    }
}
