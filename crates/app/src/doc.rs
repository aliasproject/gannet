//! Text and Markdown in the preview, as a document: blocks of styled words. The decoder process
//! makes one from the file (`excerpt`, then `plain` or `markdown`) and writes it out (`encode`),
//! so parsing whatever a file holds happens in its sandbox. The app reads it back (`decode`),
//! believing nothing it hasn't checked, and lays it out (see `docview`).

use pulldown_cmark::{Alignment, Event, Options, Parser, Tag, TagEnd};

pub const MAGIC: &[u8; 4] = b"GNPD";
/// How much of a file is shown. A file that's longer is cut at the end of a line before this.
pub const TEXT_LIMIT: usize = 128 * 1024;
/// A zero byte this near the start means it isn't text, as git decides.
const BINARY_WITHIN: usize = 8000;
/// The most table cells a document keeps. Short rows are filled out to the header's width, so a
/// few bytes of text can make many cells, and each takes more room written out than it did as text.
const MAX_CELLS: usize = TEXT_LIMIT / 4;
/// More than the decoder could ever write for `TEXT_LIMIT` of text, so anything bigger is refused:
/// eight times the text, and room for every cell with a span, whatever the text was.
pub const MAX_ENCODED: usize = 8 * TEXT_LIMIT + MAX_CELLS * 16 + 1024;

/// How a run of words looks. These combine.
pub const BOLD: u8 = 1;
pub const ITALIC: u8 = 2;
pub const CODE: u8 = 4;
pub const LINK: u8 = 8;
pub const STRUCK: u8 = 16;
const STYLES: u8 = BOLD | ITALIC | CODE | LINK | STRUCK;

#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    /// A plain text file, as it is: one block of the whole thing.
    Plain,
    Paragraph,
    /// From 1, the biggest, to 6.
    Heading(u8),
    Code,
    /// A list item's first block, with its bullet or number.
    Item(String),
    Rule,
    /// One cell of a table. A table is a run of these, row by row, and its first row is the
    /// header.
    Cell { starts: Starts, align: Align },
}

/// What a table cell is the first of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Starts {
    Nothing = 0,
    Row = 1,
    Table = 2,
}

/// Where words go across a table column, as its separator row says.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Align {
    #[default]
    Left = 0,
    Center = 1,
    Right = 2,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Span {
    pub style: u8,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Block {
    pub kind: Kind,
    /// How many quotes it's inside.
    pub quote: u8,
    /// How many lists it's inside.
    pub indent: u8,
    pub spans: Vec<Span>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Doc {
    pub blocks: Vec<Block>,
    /// The file goes on past what's here.
    pub cut: bool,
}

impl Doc {
    /// Lines in a plain text file, for the footer.
    pub fn lines(&self) -> usize {
        match self.blocks.as_slice() {
            // The newline at the end of the last line doesn't start another.
            [Block { kind: Kind::Plain, spans, .. }] => spans.iter().map(|s| s.text.lines().count()).sum::<usize>().max(1),
            _ => 0,
        }
    }
}

/// The start of a file as text, and whether there's more of it. `None` if it isn't text. `raw` is
/// what was read, which can be a little more than `TEXT_LIMIT` to tell if there's more.
pub fn excerpt(raw: &[u8]) -> Option<(String, bool)> {
    if raw[..raw.len().min(BINARY_WITHIN)].contains(&0) {
        return None;
    }
    let cut = raw.len() > TEXT_LIMIT;
    let mut kept = &raw[..raw.len().min(TEXT_LIMIT)];
    if cut {
        // At a line's end, or failing that, before a character that might be split.
        match kept.iter().rposition(|&b| b == b'\n') {
            Some(end) => kept = &kept[..end],
            None => {
                let mut end = kept.len();
                while end > 0 && kept[end - 1] & 0xc0 == 0x80 {
                    end -= 1;
                }
                kept = &kept[..end.saturating_sub(1)];
            }
        }
    }
    let kept = kept.strip_prefix(b"\xef\xbb\xbf").unwrap_or(kept);
    let mut text = String::with_capacity(kept.len());
    let lossy = String::from_utf8_lossy(kept);
    let mut chars = lossy.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {
                // Windows and old Mac line ends are line ends too.
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                text.push('\n');
            }
            '\n' | '\t' => text.push(c),
            // Controls draw as nothing useful, or as boxes.
            c if c.is_control() => text.push('\u{fffd}'),
            c => text.push(c),
        }
    }
    Some((text, cut))
}

pub fn plain(text: String, cut: bool) -> Doc {
    Doc { blocks: vec![Block { kind: Kind::Plain, quote: 0, indent: 0, spans: vec![Span { style: 0, text }] }], cut }
}

/// Markdown as a document. HTML in it is left out, as there's no way to show it as it means.
pub fn markdown(text: &str, cut: bool) -> Doc {
    let mut out = Builder::default();
    let options = Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS | Options::ENABLE_TABLES;
    // Each list being in, with the next item's number for a numbered one.
    let mut lists: Vec<Option<u64>> = Vec::new();
    let mut quote = 0u8;
    let mut style = 0u8;
    let mut styles: Vec<u8> = Vec::new();
    // An item whose first block hasn't started yet.
    let mut item: Option<String> = None;
    let indent = |lists: &Vec<Option<u64>>| lists.len().min(u8::MAX as usize) as u8;
    // The table being in: its columns' alignments, the next cell's column, and what that cell starts.
    let mut aligns: Vec<Align> = Vec::new();
    let mut column = 0usize;
    let mut starts = Starts::Nothing;
    // A row that would take the cells past `MAX_CELLS` is left out, words and all, and so is every
    // row after it. pulldown-cmark stops filling out rows after a while too, and then gives the
    // rest of the table's lines as paragraphs, which are kept.
    let (mut cells, mut dropping, mut row_left_out) = (0usize, false, false);
    let mut cut = cut;
    for event in Parser::new_ext(text, options) {
        if dropping && matches!(event, Event::Text(_) | Event::Code(_) | Event::SoftBreak | Event::HardBreak) {
            continue;
        }
        match event {
            Event::Start(tag) => match tag {
                Tag::Paragraph => out.open(item.take().map_or(Kind::Paragraph, Kind::Item), quote, indent(&lists)),
                Tag::Heading { .. } | Tag::CodeBlock(_) => {
                    // An item that starts with one of these has its bullet on a line of its own.
                    if let Some(marker) = item.take() {
                        out.open(Kind::Item(marker), quote, indent(&lists));
                        out.close();
                    }
                    let kind = match tag {
                        Tag::Heading { level, .. } => Kind::Heading(level as u8),
                        _ => Kind::Code,
                    };
                    out.open(kind, quote, indent(&lists));
                }
                Tag::BlockQuote(_) => {
                    out.close();
                    quote = quote.saturating_add(1);
                }
                Tag::List(first) => {
                    out.close();
                    // A list inside an item that hasn't said anything yet still shows the item's bullet.
                    if let Some(marker) = item.take() {
                        out.open(Kind::Item(marker), quote, indent(&lists));
                        out.close();
                    }
                    lists.push(first);
                }
                Tag::Item => {
                    out.close();
                    let marker = match lists.last_mut() {
                        Some(Some(n)) => {
                            let marker = format!("{n}.");
                            *n = n.saturating_add(1);
                            marker
                        }
                        _ => "•".to_string(),
                    };
                    item = Some(marker);
                }
                Tag::Table(columns) => {
                    // A table in an item that hasn't said anything yet has the bullet above it.
                    if let Some(marker) = item.take() {
                        out.open(Kind::Item(marker), quote, indent(&lists));
                    }
                    out.close();
                    aligns = columns
                        .iter()
                        .map(|a| match a {
                            Alignment::Center => Align::Center,
                            Alignment::Right => Align::Right,
                            Alignment::None | Alignment::Left => Align::Left,
                        })
                        .collect();
                    starts = Starts::Table;
                }
                Tag::TableHead | Tag::TableRow => {
                    column = 0;
                    row_left_out = cells + aligns.len().max(1) > MAX_CELLS;
                    if starts == Starts::Nothing {
                        starts = Starts::Row;
                    }
                }
                Tag::TableCell if row_left_out || cells == MAX_CELLS => (dropping, cut) = (true, true),
                Tag::TableCell => {
                    cells += 1;
                    let align = aligns.get(column).copied().unwrap_or_default();
                    column += 1;
                    out.open(Kind::Cell { starts, align }, quote, indent(&lists));
                    starts = Starts::Nothing;
                }
                Tag::Emphasis | Tag::Strong | Tag::Strikethrough | Tag::Link { .. } | Tag::Image { .. } => {
                    styles.push(style);
                    style |= match tag {
                        Tag::Emphasis | Tag::Image { .. } => ITALIC,
                        Tag::Strong => BOLD,
                        Tag::Strikethrough => STRUCK,
                        _ => LINK,
                    };
                }
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Paragraph | TagEnd::Heading(_) | TagEnd::CodeBlock => out.close(),
                TagEnd::TableCell => {
                    out.close();
                    dropping = false;
                }
                TagEnd::BlockQuote(_) => {
                    out.close();
                    quote = quote.saturating_sub(1);
                }
                TagEnd::List(_) => {
                    out.close();
                    lists.pop();
                }
                TagEnd::Item => {
                    // An empty item still has its bullet.
                    if let Some(marker) = item.take() {
                        out.open(Kind::Item(marker), quote, indent(&lists));
                    }
                    out.close();
                }
                TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough | TagEnd::Link | TagEnd::Image => style = styles.pop().unwrap_or(0),
                _ => {}
            },
            Event::Text(words) => {
                // A tight list's items have no paragraphs: the words are the item's.
                if let Some(marker) = item.take() {
                    out.open(Kind::Item(marker), quote, indent(&lists));
                }
                let style = if out.in_code() { CODE } else { style };
                out.push(style, &words);
            }
            Event::Code(words) => {
                if let Some(marker) = item.take() {
                    out.open(Kind::Item(marker), quote, indent(&lists));
                }
                out.push(style | CODE, &words);
            }
            Event::SoftBreak => out.push(style, " "),
            Event::HardBreak => out.push(style, "\n"),
            Event::Rule => {
                out.close();
                out.open(Kind::Rule, quote, indent(&lists));
                out.close();
            }
            Event::TaskListMarker(done) => {
                let marker = if done { "☑" } else { "☐" }.to_string();
                match &mut item {
                    Some(m) => *m = marker,
                    None => out.set_marker(marker),
                }
            }
            _ => {}
        }
    }
    out.close();
    // Code blocks end with their last line's newline, which would be an empty line.
    for block in &mut out.doc.blocks {
        if block.kind == Kind::Code
            && let Some(last) = block.spans.last_mut()
        {
            let trimmed = last.text.trim_end_matches('\n').len();
            last.text.truncate(trimmed);
        }
    }
    out.doc.cut = cut;
    out.doc
}

#[derive(Default)]
struct Builder {
    doc: Doc,
    open: Option<Block>,
}

impl Builder {
    fn open(&mut self, kind: Kind, quote: u8, indent: u8) {
        self.close();
        self.open = Some(Block { kind, quote, indent, spans: Vec::new() });
    }

    /// A block with nothing in it is dropped, unless it's a rule, a bullet or a table cell.
    fn close(&mut self) {
        if let Some(block) = self.open.take()
            && (!block.spans.is_empty() || matches!(block.kind, Kind::Rule | Kind::Item(_) | Kind::Cell { .. }))
        {
            self.doc.blocks.push(block);
        }
    }

    fn in_code(&self) -> bool {
        self.open.as_ref().is_some_and(|b| b.kind == Kind::Code)
    }

    fn set_marker(&mut self, marker: String) {
        if let Some(Block { kind: Kind::Item(m), .. }) = &mut self.open {
            *m = marker;
        }
    }

    /// Words go in the open block, or a paragraph if there isn't one, as for text in a quote.
    fn push(&mut self, style: u8, words: &str) {
        if words.is_empty() {
            return;
        }
        let block = self.open.get_or_insert_with(|| Block { kind: Kind::Paragraph, quote: 0, indent: 0, spans: Vec::new() });
        match block.spans.last_mut() {
            Some(last) if last.style == style => last.text.push_str(words),
            _ => block.spans.push(Span { style, text: words.to_string() }),
        }
    }
}

// ---- Between the processes ----

/// The first of the kinds that are table cells, and the last.
const CELL: u8 = 11;
const CELL_LAST: u8 = CELL + 3 * 3 - 1;

pub fn encode(doc: &Doc) -> Vec<u8> {
    let mut out = MAGIC.to_vec();
    out.push(u8::from(doc.cut));
    out.extend_from_slice(&(doc.blocks.len() as u32).to_le_bytes());
    for block in &doc.blocks {
        let (kind, marker) = match &block.kind {
            Kind::Plain => (0, ""),
            Kind::Paragraph => (1, ""),
            Kind::Heading(level) => (1 + *level, ""),
            Kind::Code => (8, ""),
            Kind::Item(marker) => (9, marker.as_str()),
            Kind::Rule => (10, ""),
            // What the cell starts and its alignment, in the one byte.
            Kind::Cell { starts, align } => (CELL + *starts as u8 * 3 + *align as u8, ""),
        };
        out.extend_from_slice(&[kind, block.quote, block.indent]);
        put(&mut out, marker);
        out.extend_from_slice(&(block.spans.len() as u32).to_le_bytes());
        for span in &block.spans {
            out.push(span.style);
            put(&mut out, &span.text);
        }
    }
    out
}

fn put(out: &mut Vec<u8>, text: &str) {
    out.extend_from_slice(&(text.len() as u32).to_le_bytes());
    out.extend_from_slice(text.as_bytes());
}

/// Reads back what the decoder wrote, which may have been taken over by a hostile file, so
/// nothing is believed until it's checked. `None` if any of it is wrong.
pub fn decode(bytes: &[u8]) -> Option<Doc> {
    if bytes.len() > MAX_ENCODED {
        return None;
    }
    let mut r = Reader { bytes, at: 0 };
    if r.take(4)? != MAGIC {
        return None;
    }
    let cut = match r.byte()? {
        0 => false,
        1 => true,
        _ => return None,
    };
    let count = r.count()?;
    let mut blocks = Vec::new();
    for _ in 0..count {
        let [kind, quote, indent] = [r.byte()?, r.byte()?, r.byte()?];
        let marker = r.text()?;
        let kind = match kind {
            0 => Kind::Plain,
            1 => Kind::Paragraph,
            2..=7 => Kind::Heading(kind - 1),
            8 => Kind::Code,
            9 if marker.chars().count() <= 24 => Kind::Item(marker),
            10 => Kind::Rule,
            CELL..=CELL_LAST => {
                let n = kind - CELL;
                let starts = [Starts::Nothing, Starts::Row, Starts::Table][(n / 3) as usize];
                let align = [Align::Left, Align::Center, Align::Right][(n % 3) as usize];
                Kind::Cell { starts, align }
            }
            _ => return None,
        };
        let mut spans = Vec::new();
        for _ in 0..r.count()? {
            let style = r.byte()?;
            if style & !STYLES != 0 {
                return None;
            }
            spans.push(Span { style, text: r.text()? });
        }
        blocks.push(Block { kind, quote, indent, spans });
    }
    (r.at == bytes.len()).then_some(Doc { blocks, cut })
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        let got = self.bytes.get(self.at..self.at.checked_add(n)?)?;
        self.at += n;
        Some(got)
    }

    fn byte(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }

    /// A count of things that each take at least a byte, so one bigger than what's left is a lie
    /// rather than a reason to reserve memory.
    fn count(&mut self) -> Option<usize> {
        let n = u32::from_le_bytes(self.take(4)?.try_into().ok()?) as usize;
        (n <= self.bytes.len() - self.at).then_some(n)
    }

    fn text(&mut self) -> Option<String> {
        let n = self.count()?;
        String::from_utf8(self.take(n)?.to_vec()).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(style: u8, text: &str) -> Span {
        Span { style, text: text.to_string() }
    }

    fn block(kind: Kind, spans: Vec<Span>) -> Block {
        Block { kind, quote: 0, indent: 0, spans }
    }

    #[test]
    fn text_is_cut_at_a_line_end_and_binary_is_refused() {
        assert_eq!(excerpt(b"hello\r\nthere\rold mac\n"), Some(("hello\nthere\nold mac\n".to_string(), false)));
        assert_eq!(excerpt(b"\xef\xbb\xbfbom"), Some(("bom".to_string(), false)), "the byte-order mark goes");
        assert_eq!(excerpt(b"bell\x07 and \xff"), Some(("bell\u{fffd} and \u{fffd}".to_string(), false)));
        assert_eq!(excerpt(b"abc\0def"), None);
        let mut late_zero = vec![b'a'; BINARY_WITHIN + 10];
        late_zero.push(0);
        assert!(excerpt(&late_zero).is_some(), "only a zero near the start counts");
        let mut long: Vec<u8> = Vec::new();
        while long.len() <= TEXT_LIMIT {
            long.extend_from_slice("line é\n".as_bytes());
        }
        let (text, cut) = excerpt(&long).unwrap();
        assert!(cut);
        assert!(text.len() <= TEXT_LIMIT && text.ends_with("line é"), "whole lines only");
        // One enormous line, cut where a character starts.
        let one_line = "é".repeat(TEXT_LIMIT);
        let (text, cut) = excerpt(one_line.as_bytes()).unwrap();
        assert!(cut && !text.contains('\u{fffd}') && text.len() < TEXT_LIMIT);
    }

    #[test]
    fn markdown_becomes_blocks_and_styled_words() {
        let doc = markdown("# Title\n\nSome *soft* and **strong** `code`, [a link](http://x).\n\n---\n\n```rust\nfn main() {}\n```\n", false);
        assert_eq!(
            doc.blocks,
            [
                block(Kind::Heading(1), vec![span(0, "Title")]),
                block(Kind::Paragraph, vec![span(0, "Some "), span(ITALIC, "soft"), span(0, " and "), span(BOLD, "strong"), span(0, " "), span(CODE, "code"), span(0, ", "), span(LINK, "a link"), span(0, ".")]),
                block(Kind::Rule, vec![]),
                block(Kind::Code, vec![span(CODE, "fn main() {}")]),
            ]
        );
    }

    #[test]
    fn lists_quotes_and_tasks() {
        let doc = markdown("- one\n- two\n  1. inner\n  2. next\n\n> quoted\n\n- [x] done\n- [ ] not yet\n\n3. three\n4. four\n", false);
        let summary: Vec<(Kind, u8, u8, String)> = doc.blocks.iter().map(|b| (b.kind.clone(), b.quote, b.indent, b.spans.iter().map(|s| s.text.as_str()).collect())).collect();
        let item = |m: &str| Kind::Item(m.to_string());
        assert_eq!(
            summary,
            [
                (item("•"), 0, 1, "one".into()),
                (item("•"), 0, 1, "two".into()),
                (item("1."), 0, 2, "inner".into()),
                (item("2."), 0, 2, "next".into()),
                (Kind::Paragraph, 1, 0, "quoted".into()),
                (item("☑"), 0, 1, "done".into()),
                (item("☐"), 0, 1, "not yet".into()),
                (item("3."), 0, 1, "three".into()),
                (item("4."), 0, 1, "four".into()),
            ]
        );
    }

    #[test]
    fn an_item_can_start_with_code_or_a_heading() {
        let doc = markdown("- ```\n  code\n  ```\n- # Title\n", false);
        let kinds: Vec<(Kind, u8)> = doc.blocks.iter().map(|b| (b.kind.clone(), b.indent)).collect();
        let bullet = Kind::Item("•".into());
        assert_eq!(kinds, [(bullet.clone(), 1), (Kind::Code, 1), (bullet, 1), (Kind::Heading(1), 1)]);
        assert_eq!(doc.blocks[1].spans, [span(CODE, "code")]);
    }

    fn cell(starts: Starts, align: Align, spans: Vec<Span>) -> Block {
        block(Kind::Cell { starts, align }, spans)
    }

    #[test]
    fn a_table_becomes_cells_row_by_row() {
        // A menu as it's often written: an empty header over the second column.
        let doc = markdown("# Dinner\n\n| Course | |\n|---|---|\n| Starter | Soup |\n| Main | Fish |\n\nAfter.\n", false);
        use {Align::Left, Starts::*};
        assert_eq!(
            doc.blocks,
            [
                block(Kind::Heading(1), vec![span(0, "Dinner")]),
                cell(Table, Left, vec![span(0, "Course")]),
                cell(Nothing, Left, vec![]),
                cell(Row, Left, vec![span(0, "Starter")]),
                cell(Nothing, Left, vec![span(0, "Soup")]),
                cell(Row, Left, vec![span(0, "Main")]),
                cell(Nothing, Left, vec![span(0, "Fish")]),
                block(Kind::Paragraph, vec![span(0, "After.")]),
            ]
        );
    }

    #[test]
    fn table_cells_keep_their_alignment_and_styles() {
        let doc = markdown("| a | b | c | d |\n|:--|:-:|--:|---|\n| **bold** *it* | `code` | [link](x) | ~~gone~~ |\n", false);
        let aligns: Vec<Align> = doc
            .blocks
            .iter()
            .map(|b| match b.kind {
                Kind::Cell { align, .. } => align,
                _ => panic!("not a cell: {b:?}"),
            })
            .collect();
        use Align::*;
        assert_eq!(aligns, [Left, Center, Right, Left, Left, Center, Right, Left]);
        assert_eq!(doc.blocks[4].spans, [span(BOLD, "bold"), span(0, " "), span(ITALIC, "it")]);
        assert_eq!(doc.blocks[5].spans, [span(CODE, "code")]);
        assert_eq!(doc.blocks[6].spans, [span(LINK, "link")]);
        assert_eq!(doc.blocks[7].spans, [span(STRUCK, "gone")]);
    }

    #[test]
    fn ragged_rows_are_squared_to_the_header() {
        // A short row gets empty cells, and a long one loses what's past the header's columns.
        let doc = markdown("| a | b | c |\n|---|---|---|\n| 1 |\n| 1 | 2 | 3 | 4 |\n\n| next |\n|---|\n| x |\n", false);
        let summary: Vec<(Starts, String)> = doc
            .blocks
            .iter()
            .map(|b| match b.kind {
                Kind::Cell { starts, .. } => (starts, b.spans.iter().map(|s| s.text.as_str()).collect()),
                _ => panic!("not a cell: {b:?}"),
            })
            .collect();
        use Starts::*;
        let want = [(Table, "a"), (Nothing, "b"), (Nothing, "c"), (Row, "1"), (Nothing, ""), (Nothing, ""), (Row, "1"), (Nothing, "2"), (Nothing, "3"), (Table, "next"), (Row, "x")];
        assert_eq!(summary, want.map(|(s, t)| (s, t.to_string())), "a second table starts its own");
    }

    #[test]
    fn a_wide_header_over_many_short_rows_cant_make_endless_cells() {
        // Every short row is filled out to the header's 1000 columns.
        let mut text = format!("{}|\n{}|\n", "| h ".repeat(1000), "|---".repeat(1000));
        while text.len() < TEXT_LIMIT - 10 {
            text.push_str("| x |\n");
        }
        let cells = |doc: &Doc| doc.blocks.iter().filter(|b| matches!(b.kind, Kind::Cell { .. })).count();
        let doc = markdown(&text, false);
        // Whole rows, as many as fit.
        assert_eq!(cells(&doc), MAX_CELLS / 1000 * 1000);
        assert!(doc.cut, "said to go on past what's shown");
        assert!(encode(&doc).len() <= MAX_ENCODED, "and it goes between the processes");
        // The words of cells left out don't turn up anywhere else, and what follows is still there.
        let doc = markdown(&format!("{text}\nafter\n"), false);
        assert_eq!(cells(&doc), MAX_CELLS / 1000 * 1000);
        assert!(doc.blocks.iter().all(|b| b.kind == Kind::Paragraph || matches!(b.kind, Kind::Cell { .. }) && b.spans.iter().all(|s| s.text == "h" || s.text == "x")), "{:?}", &doc.blocks[MAX_CELLS - 2..]);
        assert_eq!(doc.blocks.last().unwrap().spans, [span(0, "after")]);
        // As many cells as can be, then the rest of the text as what writes out biggest for its
        // size: empty bullets.
        let mut text = format!("{}|\n{}|\n", "|".repeat(1000), "|-".repeat(1000));
        for _ in 0..40 {
            text.push_str("|x\n");
        }
        while text.len() < TEXT_LIMIT - 2 {
            text.push_str("-\n");
        }
        let doc = markdown(&text, false);
        assert_eq!(cells(&doc), MAX_CELLS / 1000 * 1000);
        assert!(encode(&doc).len() <= MAX_ENCODED, "{} bytes", encode(&doc).len());
    }

    #[test]
    fn a_table_in_a_list_or_a_quote_is_in_it() {
        let doc = markdown("> | q |\n> |---|\n> | 1 |\n\n- | i |\n  |---|\n", false);
        let summary: Vec<(Kind, u8, u8)> = doc.blocks.iter().map(|b| (b.kind.clone(), b.quote, b.indent)).collect();
        let cell = |starts| Kind::Cell { starts, align: Align::Left };
        assert_eq!(summary, [(cell(Starts::Table), 1, 0), (cell(Starts::Row), 1, 0), (Kind::Item("•".into()), 0, 1), (cell(Starts::Table), 0, 1)]);
    }

    #[test]
    fn html_is_left_out_and_breaks_are_kept() {
        let doc = markdown("<div align=\"center\">\n<img src=\"x\">\n</div>\n\nline one  \nline two\nsame line\n", false);
        assert_eq!(doc.blocks, [block(Kind::Paragraph, vec![span(0, "line one\nline two same line")])]);
    }

    #[test]
    fn a_document_goes_between_the_processes_whole() {
        let doc = Doc { blocks: markdown("# Hi\n\n- *a* `b`\n\n> c\n\n---\n\n| x | |\n|:-:|--:|\n| **1** | 2 |\n", true).blocks, cut: true };
        assert_eq!(decode(&encode(&doc)), Some(doc.clone()));
        let text = plain("x\ny".to_string(), false);
        assert_eq!(decode(&encode(&text)).unwrap().lines(), 2);
        assert_eq!(plain("x\ny\n".to_string(), false).lines(), 2, "a last newline ends a line");
        assert_eq!(plain("x\n\n".to_string(), false).lines(), 2);
        assert_eq!(plain(String::new(), false).lines(), 1);
    }

    #[test]
    fn nothing_from_the_decoder_is_believed_unchecked() {
        let good = encode(&markdown("# Hi\n\n*there*\n", false));
        assert!(decode(&good).is_some());
        // Cut short anywhere, or with something extra on the end.
        for end in 0..good.len() {
            assert!(decode(&good[..end]).is_none(), "cut at {end}");
        }
        assert!(decode(&[good.as_slice(), b"x"].concat()).is_none());
        // A count far bigger than what follows.
        let mut lie = good.clone();
        lie[5..9].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(decode(&lie).is_none());
        // Bytes that aren't text, a style that doesn't exist, a kind that doesn't exist.
        let text_at = good.len() - "there".len();
        let mut not_text = good.clone();
        not_text[text_at] = 0xff;
        assert!(decode(&not_text).is_none());
        let mut odd_style = good.clone();
        odd_style[text_at - 5] = 0x80;
        assert!(decode(&odd_style).is_none());
        let mut odd_kind = good;
        odd_kind[9] = 99;
        assert!(decode(&odd_kind).is_none());
        // Every kind of cell there is reads back, and none past them.
        let one = |kind: u8| [MAGIC.as_slice(), &[0], &1u32.to_le_bytes(), &[kind, 0, 0], &0u32.to_le_bytes(), &0u32.to_le_bytes()].concat();
        assert!((CELL..=CELL_LAST).all(|k| decode(&one(k)).is_some_and(|d| matches!(d.blocks[0].kind, Kind::Cell { .. }))));
        assert!(decode(&one(CELL_LAST + 1)).is_none());
        // And each one is the cell it was.
        for starts in [Starts::Nothing, Starts::Row, Starts::Table] {
            for align in [Align::Left, Align::Center, Align::Right] {
                let doc = Doc { blocks: vec![block(Kind::Cell { starts, align }, vec![])], cut: false };
                assert_eq!(decode(&encode(&doc)), Some(doc));
            }
        }
    }
}
