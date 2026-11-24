//! Showing a document (see `doc`) in the preview: the fonts it's set in, laying its words out in
//! lines to the window's width, and drawing the lines that are in view.
//!
//! A plain text file is set in a monospaced font and wrapped by counting columns, so even a long
//! one lays out without measuring a word. Markdown is measured word by word.

use std::borrow::Cow;
use std::path::{Path, PathBuf};

use crate::canvas::{Canvas, Rect};
use crate::doc::{self, Align, Block, Doc, Kind, Starts};
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
            // Bold and italic are the interface's font, which body text is in, so a document reads
            // in one typeface.
            Role::Bold => "monospace:bold",
            Role::Italic => "monospace:italic",
        }
    }

    /// The cache file that remembers what fontconfig said, so the next document doesn't wait.
    fn cache_name(self) -> &'static str {
        match self {
            Role::Mono => "font-mono",
            // Named for the font asked for, so one remembered when another was asked for isn't used.
            Role::Bold => "font-bold-monospace",
            Role::Italic => "font-italic-monospace",
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
            let mut t = Text::new(px * scale as f32, text::UI_HINTED && scale < 1.5);
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
            t.set_size(px * scale as f32, text::UI_HINTED && scale < 1.5);
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
    /// Part of a table row, which runs between `left` and `right`. The header row is washed, and
    /// rules go above the table and below each row.
    Table { left: i32, right: i32, wash: bool, above: bool, below: bool },
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
    let mut i = 0;
    while let Some(block) = doc.blocks.get(i) {
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
        i += 1;
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
            Kind::Cell { .. } => {
                let cells = 1 + doc.blocks[i..].iter().take_while(|b| matches!(b.kind, Kind::Cell { starts, .. } if starts != Starts::Table)).count();
                y = table(&doc.blocks[i - 1..i - 1 + cells], left, right, y, fonts, &mut out.lines);
                i += cells - 1;
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

/// Lays out a table's cells from `y`, returning where the next block starts. Its columns are as
/// wide as their words when there's room, and share what room there is when there isn't.
fn table(cells: &[Block], left: i32, right: i32, mut y: i32, fonts: &mut Fonts, lines: &mut Vec<Line>) -> i32 {
    let (pad, vpad) = (fonts.px(10.0), fonts.px(6.0));
    let mut rows: Vec<Vec<Cow<Block>>> = Vec::new();
    for cell in cells {
        if rows.is_empty() || matches!(cell.kind, Kind::Cell { starts: Starts::Row, .. }) {
            rows.push(Vec::new());
        }
        // The header's words are bold.
        let cell = if rows.len() == 1 {
            let mut bold = cell.clone();
            bold.spans.iter_mut().for_each(|s| s.style |= doc::BOLD);
            Cow::Owned(bold)
        } else {
            Cow::Borrowed(cell)
        };
        if let Some(row) = rows.last_mut() {
            row.push(cell);
        }
    }
    let quote = cells.first().map_or(0, |c| c.quote);
    // How wide each column's words are all on one line, and its longest word.
    let count = rows.iter().map(Vec::len).max().unwrap_or(0);
    let (mut natural, mut longest) = (vec![0; count], vec![0; count]);
    for row in &rows {
        for (c, cell) in row.iter().enumerate() {
            let (w, word) = measure_cell(cell, fonts);
            natural[c] = natural[c].max(w + pad * 2);
            longest[c] = longest[c].max(word + pad * 2);
        }
    }
    let widths = share(&natural, &longest, right - left);
    let table_right = left + widths.iter().sum::<i32>();
    for (r, row) in rows.iter().enumerate() {
        // Each cell's lines, side by side: a row's nth line is every cell's nth line.
        let mut merged: Vec<Line> = Vec::new();
        let mut x = left;
        for (cell, &w) in row.iter().zip(&widths) {
            // A column narrower than its padding gives up some of it, so its words stay inside.
            let inset = pad.min(w / 4);
            let (from, to) = (x + inset, (x + w - inset).max(x + inset + 1));
            x += w;
            let mut own = Vec::new();
            flow(cell, from, to, 0, fonts, &mut own);
            for (k, mut line) in own.into_iter().enumerate() {
                let shift = match cell.kind {
                    Kind::Cell { align: Align::Left, .. } => 0.0,
                    Kind::Cell { align, .. } => {
                        let ink = line.pieces.iter().map(|p| p.x + fonts.get(p.face).measure(p.text.trim_end_matches(' '))).fold(from as f32, f32::max);
                        let spare = (to as f32 - ink).max(0.0);
                        if align == Align::Center { (spare / 2.0).floor() } else { spare }
                    }
                    _ => 0.0,
                };
                line.pieces.iter_mut().for_each(|p| p.x += shift);
                match merged.get_mut(k) {
                    Some(m) => {
                        m.h = m.h.max(line.h);
                        m.baseline = m.baseline.max(line.baseline);
                        m.pieces.append(&mut line.pieces);
                    }
                    None => merged.push(line),
                }
            }
        }
        // A row of empty cells is still a row.
        if merged.is_empty() {
            let t = fonts.get(Face::Body);
            merged.push(Line { y: 0, h: t.line_height().ceil() as i32, baseline: t.ascent.round() as i32, quote, mark: Mark::None, marker: None, pieces: Vec::new() });
        }
        let last = merged.len() - 1;
        for (k, mut line) in merged.into_iter().enumerate() {
            // The row's padding goes to its first and last lines, as a code block's does.
            if k == 0 {
                line.h += vpad;
                line.baseline += vpad;
            }
            if k == last {
                line.h += vpad;
            }
            line.y = y;
            line.quote = quote;
            line.mark = Mark::Table { left, right: table_right, wash: r == 0, above: r == 0 && k == 0, below: k == last };
            y += line.h;
            lines.push(line);
        }
    }
    y
}

/// How wide a cell's words are on one line, and its widest word, in pixels.
fn measure_cell(cell: &Block, fonts: &mut Fonts) -> (i32, i32) {
    let (mut line, mut widest, mut word_most) = (0.0f32, 0.0f32, 0.0f32);
    for span in &cell.spans {
        let t = fonts.get(face_for(&cell.kind, span.style));
        for word in words(&span.text) {
            if word == "\n" {
                line = 0.0;
                continue;
            }
            let ink = t.measure(word.trim_end_matches(' '));
            widest = widest.max(line + ink);
            word_most = word_most.max(ink);
            line += t.measure(word);
        }
    }
    (widest.ceil() as i32, word_most.ceil() as i32)
}

/// Columns' widths in `room`: as wide as their words when they all fit. Otherwise each keeps its
/// longest word whole and the rest of the room goes to the columns that wanted more, by how much
/// more. If even the longest words don't fit, every column gives up room alike, and words break.
fn share(natural: &[i32], longest: &[i32], room: i32) -> Vec<i32> {
    let room = room.max(natural.len() as i32);
    let want: i64 = natural.iter().map(|&w| w as i64).sum();
    if want <= room as i64 {
        return natural.to_vec();
    }
    let least: Vec<i64> = natural.iter().zip(longest).map(|(&n, &l)| l.min(n).max(1) as i64).collect();
    let floor: i64 = least.iter().sum();
    if floor >= room as i64 {
        return least.iter().map(|&l| (l * room as i64 / floor).max(1) as i32).collect();
    }
    let (spare, extra) = (room as i64 - floor, want - floor);
    natural.iter().zip(&least).map(|(&n, &l)| (l + (n as i64 - l) * spare / extra.max(1)) as i32).collect()
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
            Mark::Table { left, right, wash, above, below } => {
                let (x, w, thin) = (area.x + left, right - left, fonts.px(1.0).max(1));
                if wash {
                    c.fill(Rect::new(x, y, w, line.h), ink.code);
                }
                if above {
                    c.fill(Rect::new(x, y, w, thin), ink.rule);
                }
                if below {
                    c.fill(Rect::new(x, y + line.h - thin, w, thin), ink.rule);
                }
            }
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
            // Code among words gets a little box of its own, as does code in a table cell, whose
            // line holds the other cells' words too.
            let among = match line.mark {
                Mark::Code(..) => false,
                Mark::Table { .. } => true,
                _ => line.pieces.len() > 1,
            };
            if piece.style & doc::CODE != 0 && among && piece.face == Face::Mono {
                let w = t.measure(piece.text.trim_end_matches(' ')).ceil() as i32;
                let h = t.line_height().ceil() as i32;
                // On the header's wash, which is the code colour already, it takes the rules' colour.
                let fill = if matches!(line.mark, Mark::Table { wash: true, .. }) { ink.rule } else { ink.code };
                c.rrect(Rect::new(x as i32 - 3, baseline - t.ascent.round() as i32 - 1, w + 6, h + 2), 4, fill);
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

    #[test]
    fn documents_are_in_the_interface_font_throughout() {
        // Body text is in the interface's font, so bold, italic and headings are too.
        assert!(Role::ALL.iter().all(|r| r.pattern().starts_with(text::UI_FONT)), "{:?}", Role::ALL.map(Role::pattern));
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
    fn columns_share_the_room_when_there_isnt_enough() {
        assert_eq!(share(&[100, 50], &[40, 50], 400), [100, 50], "as wide as their words when there's room");
        // Each keeps its longest word, and the rest goes by how much more each wanted.
        assert_eq!(share(&[300, 100], &[50, 50], 200), [133, 66]);
        let tight = share(&[300, 100], &[200, 100], 150);
        assert_eq!(tight, [100, 50], "even the longest words don't fit: both give up alike");
        assert!(share(&[10; 50], &[10; 50], 0).iter().all(|&w| w >= 1), "no column vanishes");
        assert_eq!(share(&[], &[], 100), Vec::<i32>::new());
    }

    /// Where the words in a laid-out line are: (x, text), left to right.
    fn pieces(line: &Line) -> Vec<(f32, &str)> {
        line.pieces.iter().map(|p| (p.x, p.text.as_str())).collect()
    }

    #[test]
    fn a_table_lays_out_as_a_grid_with_its_header_bold() {
        let Some(mut fonts) = fonts() else { return };
        let doc = doc::markdown("# Dinner\n\n| Course | |\n|---|---|\n| Starter | Soup |\n| Main | Fish |\n\nAfter.\n", false);
        let layout = lay_out(&doc, 800, &mut fonts);
        let rows: Vec<&Line> = layout.lines.iter().filter(|l| matches!(l.mark, Mark::Table { .. })).collect();
        assert_eq!(rows.len(), 3, "a line for each row");
        assert!(rows.windows(2).all(|w| w[0].y + w[0].h == w[1].y), "rows follow one another");
        assert!(rows[0].pieces.iter().all(|p| p.face == Face::Bold), "the header is bold");
        assert!(rows[1].pieces.iter().all(|p| p.face == Face::Body));
        assert_eq!(rows[1].pieces.len(), 2, "both cells of a row are on its line");
        // The second column starts in the same place on every row, past the first column's words.
        let (starter, soup) = (rows[1].pieces[0].x, rows[1].pieces[1].x);
        assert_eq!(rows[2].pieces[1].x, soup);
        assert!(soup > starter + fonts.get(Face::Body).measure("Starter"));
        let Mark::Table { left, right, .. } = rows[0].mark else { unreachable!() };
        assert!(right - left < 400, "as wide as its words, not the window");
        let marks: Vec<(bool, bool, bool)> = rows.iter().map(|l| match l.mark {
            Mark::Table { wash, above, below, .. } => (wash, above, below),
            _ => unreachable!(),
        }).collect();
        assert_eq!(marks, [(true, true, true), (false, false, true), (false, false, true)], "the header's washed, with rules above and between");
        assert_eq!(pieces(layout.lines.last().unwrap()), [(left as f32, "After.")], "what follows is back to a paragraph");
    }

    #[test]
    fn a_narrow_table_wraps_within_its_columns_and_follows_alignment() {
        let Some(mut fonts) = fonts() else { return };
        let long = "a long description of the dish that goes on for some while ".repeat(4);
        let doc = doc::markdown(&format!("| Dish | Price |\n|---|--:|\n| {long} | 12 |\n| x | 1234 |\n"), false);
        let width = 360;
        let layout = lay_out(&doc, width, &mut fonts);
        let right_edge = (width - fonts.px(20.0)) as f32 + 1.0;
        for line in &layout.lines {
            for p in &line.pieces {
                assert!(p.x + fonts.get(p.face).measure(p.text.trim_end()) <= right_edge, "{:?} is inside the window", p.text);
            }
            let Mark::Table { right, .. } = line.mark else { panic!("not in the table") };
            assert!(right <= width - fonts.px(20.0), "the table fits");
        }
        assert!(layout.lines.len() > 4, "the long cell wraps over lines");
        // The price column is right-aligned: its numbers end in the same place.
        let ends: Vec<f32> = layout.lines.iter().filter_map(|l| l.pieces.iter().find(|p| p.text.trim().chars().all(|c| c.is_ascii_digit()))).map(|p| p.x + fonts.get(p.face).measure(&p.text)).collect();
        assert_eq!(ends.len(), 2);
        assert!((ends[0] - ends[1]).abs() < 1.0, "{ends:?}");
    }

    #[test]
    fn empty_and_ragged_tables_still_lay_out() {
        let Some(mut fonts) = fonts() else { return };
        // A header with nothing in it, and cells a hostile decoder might send in any order.
        let doc = doc::markdown("| | |\n|---|---|\n", false);
        let layout = lay_out(&doc, 600, &mut fonts);
        assert_eq!(layout.lines.len(), 1, "an empty row is still a row");
        assert!(layout.lines[0].h > 0);
        let cell = |starts| Block { kind: Kind::Cell { starts, align: doc::Align::Center }, quote: 0, indent: 0, spans: vec![doc::Span { style: 0, text: "x".into() }] };
        let odd = Doc { blocks: vec![cell(Starts::Nothing), cell(Starts::Row), cell(Starts::Nothing), cell(Starts::Nothing), cell(Starts::Table)], cut: false };
        let layout = lay_out(&odd, 10, &mut fonts);
        assert_eq!(layout.lines.len(), 3, "two tables, the first of two rows");
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
