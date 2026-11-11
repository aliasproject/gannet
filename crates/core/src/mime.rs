//! What kind of file something is, from the shared-mime-info database every desktop installs:
//! the name first (`globs2`), then the first bytes (`magic`). Also the type's name for people
//! ("PDF document") and the generic icon it belongs with. Reads only the database files it's
//! given and the bytes it's handed: opening the file being described is the caller's business.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// A database file bigger than this isn't one `update-mime-database` wrote.
const MAX_DB_FILE: u64 = 8 * 1024 * 1024;
/// A type's description file is a few kilobytes of translations.
const MAX_TYPE_FILE: u64 = 1024 * 1024;
/// How far into a file magic rules look. The real database stays well inside this.
pub const HEAD: usize = 4096;

pub const OCTET_STREAM: &str = "application/octet-stream";
pub const PLAIN_TEXT: &str = "text/plain";
pub const EMPTY: &str = "application/x-zerosize";
pub const FOLDER: &str = "inode/directory";

struct Glob {
    weight: u32,
    mime: String,
    pattern: Vec<u8>,
    case_sensitive: bool,
}

/// One magic test: bytes to find at an offset, maybe masked, maybe anywhere in a range.
struct Test {
    indent: u32,
    start: usize,
    range: usize,
    value: Vec<u8>,
    mask: Option<Vec<u8>>,
}

struct Section {
    priority: u32,
    mime: String,
    tests: Vec<Test>,
}

#[derive(Default)]
pub struct MimeDb {
    /// `*.ext` patterns, the vast majority, by lowercased extension.
    by_ext: HashMap<Vec<u8>, Vec<usize>>,
    globs: Vec<Glob>,
    /// The other patterns, tried one by one.
    other: Vec<usize>,
    magic: Vec<Section>,
    generic_icons: HashMap<String, String>,
    /// The `mime` folders, first wins, for looking up descriptions.
    dirs: Vec<PathBuf>,
}

impl MimeDb {
    /// Reads `globs2`, `magic` and `generic-icons` from each `mime` folder (`$XDG_DATA_HOME/mime`,
    /// then each of `$XDG_DATA_DIRS`), the first folder's entries winning. Folders that don't
    /// exist or don't parse are left out, so this always gives a database, if an empty one.
    pub fn load(dirs: &[PathBuf]) -> Self {
        let mut db = MimeDb { dirs: dirs.to_vec(), ..Default::default() };
        for dir in dirs {
            if let Some(text) = read_capped(&dir.join("globs2"), MAX_DB_FILE) {
                db.add_globs(&text);
            }
            if let Some(bytes) = read_capped(&dir.join("magic"), MAX_DB_FILE) {
                db.add_magic(&bytes);
            }
            if let Some(text) = read_capped(&dir.join("generic-icons"), MAX_DB_FILE) {
                for line in String::from_utf8_lossy(&text).lines() {
                    if let Some((mime, icon)) = line.split_once(':') {
                        db.generic_icons.entry(mime.to_string()).or_insert_with(|| icon.to_string());
                    }
                }
            }
        }
        // Highest priority first, keeping the files' own order otherwise.
        db.magic.sort_by_key(|s| std::cmp::Reverse(s.priority));
        db
    }

    pub fn is_empty(&self) -> bool {
        self.globs.is_empty() && self.magic.is_empty()
    }

    fn add_globs(&mut self, text: &[u8]) {
        for line in text.split(|&b| b == b'\n') {
            if line.is_empty() || line[0] == b'#' {
                continue;
            }
            let mut parts = line.splitn(4, |&b| b == b':');
            let (Some(weight), Some(mime), Some(pattern)) = (parts.next(), parts.next(), parts.next()) else { continue };
            let Some(weight) = std::str::from_utf8(weight).ok().and_then(|w| w.parse().ok()) else { continue };
            let (Ok(mime), false) = (std::str::from_utf8(mime), pattern.is_empty()) else { continue };
            let case_sensitive = parts.next().is_some_and(|flags| flags.split(|&b| b == b',').any(|f| f == b"cs"));
            let pattern = if case_sensitive { pattern.to_vec() } else { pattern.to_ascii_lowercase() };
            let index = self.globs.len();
            match pattern.strip_prefix(b"*.") {
                Some(ext) if !case_sensitive && !ext.iter().any(|b| b"*?[".contains(b)) => self.by_ext.entry(ext.to_vec()).or_default().push(index),
                _ => self.other.push(index),
            }
            self.globs.push(Glob { weight, mime: mime.to_string(), pattern, case_sensitive });
        }
    }

    fn add_magic(&mut self, bytes: &[u8]) {
        let Some(mut rest) = bytes.strip_prefix(b"MIME-Magic\0\n") else { return };
        let mut current: Option<Section> = None;
        while !rest.is_empty() {
            if rest[0] == b'[' {
                let Some(end) = rest.iter().position(|&b| b == b'\n') else { break };
                let header = &rest[1..end];
                rest = &rest[end + 1..];
                if let Some(s) = current.take() {
                    self.magic.push(s);
                }
                let header = header.strip_suffix(b"]").unwrap_or(header);
                let Some((priority, mime)) = std::str::from_utf8(header).ok().and_then(|h| h.split_once(':')) else { continue };
                current = priority.parse().ok().map(|priority| Section { priority, mime: mime.to_string(), tests: Vec::new() });
                continue;
            }
            match parse_test(rest) {
                Some((test, after)) => {
                    if let Some(s) = current.as_mut() {
                        s.tests.push(test);
                    }
                    rest = after;
                }
                // A line this can't read: the rest of the file can't be trusted to line up.
                None => break,
            }
        }
        if let Some(s) = current {
            self.magic.push(s);
        }
    }

    /// The type of a file called `name` that starts with `head` (up to [`HEAD`] bytes; `None`
    /// when its contents couldn't be read). Never fails: an unknown file is `text/plain` when it
    /// looks like text, and `application/octet-stream` otherwise.
    pub fn guess(&self, name: &[u8], head: Option<&[u8]>) -> &str {
        let candidates = self.by_name(name);
        if let [only] = candidates.as_slice() {
            return only;
        }
        if let Some(head) = head {
            if head.is_empty() {
                return candidates.first().copied().unwrap_or(EMPTY);
            }
            let found = if candidates.is_empty() { self.by_magic(head, None) } else { self.by_magic(head, Some(&candidates)) };
            if let Some(found) = found {
                return found;
            }
        }
        if let Some(first) = candidates.first() {
            return first;
        }
        match head {
            Some(head) if looks_like_text(head) => PLAIN_TEXT,
            _ => OCTET_STREAM,
        }
    }

    /// The types a name's best patterns give: the highest weight, then the longest pattern, then
    /// a case-sensitive one.
    /// More than one when the name alone can't decide.
    fn by_name<'a>(&'a self, name: &[u8]) -> Vec<&'a str> {
        let lower = name.to_ascii_lowercase();
        let mut best: Option<(u32, usize, bool)> = None;
        let mut found: Vec<&'a str> = Vec::new();
        let mut consider = |g: &'a Glob| {
            let subject = if g.case_sensitive { name } else { &lower };
            if !glob_match(&g.pattern, subject) {
                return;
            }
            // At equal weight and length, a pattern that had to match case beats one that didn't.
            let rank = (g.weight, g.pattern.len(), g.case_sensitive);
            if best.is_none_or(|b| rank > b) {
                best = Some(rank);
                found.clear();
            }
            if best == Some(rank) && !found.contains(&g.mime.as_str()) {
                found.push(&g.mime);
            }
        };
        // Every `*.ext` a name could end in, so `x.tar.gz` finds `*.tar.gz` as well as `*.gz`.
        let mut at = 0;
        while let Some(dot) = lower[at..].iter().position(|&b| b == b'.') {
            at += dot + 1;
            for &i in self.by_ext.get(&lower[at..]).map_or(&[][..], Vec::as_slice) {
                consider(&self.globs[i]);
            }
        }
        for &i in &self.other {
            consider(&self.globs[i]);
        }
        found
    }

    /// The highest-priority magic section that matches, from among `only` if given.
    fn by_magic(&self, head: &[u8], only: Option<&[&str]>) -> Option<&str> {
        self.magic.iter().filter(|s| only.is_none_or(|o| o.contains(&s.mime.as_str()))).find(|s| section_matches(&s.tests, head)).map(|s| s.mime.as_str())
    }

    /// The name of the generic icon for a type, like `audio-x-generic`, from the database or
    /// from its media type.
    pub fn generic_icon(&self, mime: &str) -> String {
        if let Some(icon) = self.generic_icons.get(mime) {
            return icon.clone();
        }
        let media = mime.split('/').next().unwrap_or("");
        format!("{media}-x-generic")
    }

    /// The type's name for people, in `lang` (like `de_DE.UTF-8`) when the database has it.
    pub fn describe(&self, mime: &str, lang: Option<&str>) -> Option<String> {
        // A type comes from the database or from code here, but keep it to a file name anyway.
        let (media, sub) = mime.split_once('/')?;
        let plain = |s: &str| !s.is_empty() && !s.starts_with('.') && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"+-._".contains(&b));
        if !plain(media) || !plain(sub) {
            return None;
        }
        self.dirs.iter().find_map(|dir| {
            let text = read_capped(&dir.join(media).join(format!("{sub}.xml")), MAX_TYPE_FILE)?;
            comment(&String::from_utf8_lossy(&text), lang)
        })
    }
}

fn read_capped(path: &Path, max: u64) -> Option<Vec<u8>> {
    use std::io::Read;
    let file = std::fs::File::open(path).ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut out = Vec::new();
    file.take(max + 1).read_to_end(&mut out).ok()?;
    (out.len() as u64 <= max).then_some(out)
}

/// Text when nothing in it is a control character other than the usual whitespace, and the
/// part that's there is UTF-8, allowing for a character cut off at the end.
pub fn looks_like_text(head: &[u8]) -> bool {
    let valid = match std::str::from_utf8(head) {
        Ok(_) => true,
        Err(e) => e.error_len().is_none(),
    };
    valid && !head.iter().any(|&b| b < 0x20 && !b"\t\n\r\x0c\x1b".contains(&b) || b == 0x7f)
}

/// `[indent]>start=LLvalue[&mask][~size][+range]\n`, numbers in decimal, `LL` big-endian.
fn parse_test(line: &[u8]) -> Option<(Test, &[u8])> {
    let number = |s: &[u8]| -> Option<(usize, usize)> {
        let n = s.iter().take_while(|b| b.is_ascii_digit()).count();
        Some((std::str::from_utf8(&s[..n]).ok()?.parse().ok()?, n))
    };
    let mut at = 0;
    let (indent, n) = if line[0] == b'>' { (0, 0) } else { number(line)? };
    at += n;
    if line.get(at) != Some(&b'>') {
        return None;
    }
    at += 1;
    let (start, n) = number(&line[at..])?;
    at += n;
    if line.get(at) != Some(&b'=') {
        return None;
    }
    at += 1;
    let len = u16::from_be_bytes([*line.get(at)?, *line.get(at + 1)?]) as usize;
    at += 2;
    let value = line.get(at..at + len)?.to_vec();
    at += len;
    let mut mask = None;
    if line.get(at) == Some(&b'&') {
        mask = Some(line.get(at + 1..at + 1 + len)?.to_vec());
        at += 1 + len;
    }
    let mut word = 1;
    if line.get(at) == Some(&b'~') {
        let (w, n) = number(&line[at + 1..])?;
        word = w;
        at += 1 + n;
    }
    let mut range = 1;
    if line.get(at) == Some(&b'+') {
        let (r, n) = number(&line[at + 1..])?;
        range = r.max(1);
        at += 1 + n;
    }
    if line.get(at) != Some(&b'\n') {
        return None;
    }
    let (mut value, mut mask) = (value, mask);
    // Values of 16 or 32-bit words are written big-endian, so on a little-endian machine they
    // match the file's bytes swapped.
    if cfg!(target_endian = "little") && (word == 2 || word == 4) && value.len() % word == 0 {
        for chunk in value.chunks_mut(word) {
            chunk.reverse();
        }
        if let Some(m) = mask.as_mut() {
            for chunk in m.chunks_mut(word) {
                chunk.reverse();
            }
        }
    }
    Some((Test { indent: indent as u32, start, range, value, mask }, &line[at + 1..]))
}

fn test_matches(t: &Test, head: &[u8]) -> bool {
    let len = t.value.len();
    if len == 0 {
        return false;
    }
    (t.start..t.start.saturating_add(t.range)).any(|at| {
        let Some(bytes) = head.get(at..at + len) else { return false };
        match &t.mask {
            None => bytes == t.value.as_slice(),
            Some(mask) => bytes.iter().zip(mask).zip(&t.value).all(|((b, m), v)| b & m == v & m),
        }
    })
}

/// A section matches when one of its top-level tests does, along with one of that test's
/// children if it has any, and so on down.
fn section_matches(tests: &[Test], head: &[u8]) -> bool {
    fn matches_from(tests: &[Test], i: usize, head: &[u8]) -> bool {
        if !test_matches(&tests[i], head) {
            return false;
        }
        let indent = tests[i].indent;
        let mut children = (i + 1..tests.len()).take_while(|&j| tests[j].indent > indent).filter(|&j| tests[j].indent == indent + 1).peekable();
        children.peek().is_none() || children.any(|j| matches_from(tests, j, head))
    }
    (0..tests.len()).filter(|&i| tests[i].indent == 0).any(|i| matches_from(tests, i, head))
}

/// Shell-style patterns: `*`, `?` and `[...]` classes, with `!` negating a class.
fn glob_match(pattern: &[u8], name: &[u8]) -> bool {
    let (mut p, mut n) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while n < name.len() {
        if p < pattern.len() {
            match pattern[p] {
                b'*' => {
                    star = Some((p, n));
                    p += 1;
                    continue;
                }
                b'?' => {
                    p += 1;
                    n += 1;
                    continue;
                }
                b'[' => {
                    if let Some((matched, end)) = class_match(&pattern[p..], name[n])
                        && matched
                    {
                        p += end;
                        n += 1;
                        continue;
                    }
                }
                c if c == name[n] => {
                    p += 1;
                    n += 1;
                    continue;
                }
                _ => {}
            }
        }
        match star {
            Some((sp, sn)) => {
                p = sp + 1;
                n = sn + 1;
                star = Some((sp, sn + 1));
            }
            None => return false,
        }
    }
    pattern[p..].iter().all(|&c| c == b'*')
}

/// Whether `c` is in the class at the start of `pattern`, and the class's length.
fn class_match(pattern: &[u8], c: u8) -> Option<(bool, usize)> {
    let mut i = 1;
    let negate = matches!(pattern.get(i), Some(b'!' | b'^'));
    if negate {
        i += 1;
    }
    let mut matched = false;
    let mut first = true;
    loop {
        let ch = *pattern.get(i)?;
        if ch == b']' && !first {
            return Some((matched != negate, i + 1));
        }
        first = false;
        if pattern.get(i + 1) == Some(&b'-') && pattern.get(i + 2).is_some_and(|&e| e != b']') {
            let end = pattern[i + 2];
            matched |= (ch..=end).contains(&c);
            i += 3;
        } else {
            matched |= ch == c;
            i += 1;
        }
    }
}

/// The `<comment>` for `lang` from a type's XML, or the untranslated one.
fn comment(xml: &str, lang: Option<&str>) -> Option<String> {
    let wanted: Vec<String> = lang
        .map(|l| {
            let l = l.split(['.', '@']).next().unwrap_or("");
            let mut out = vec![l.to_string(), l.replace('_', "-")];
            if let Some((language, _)) = l.split_once('_') {
                out.push(language.to_string());
            }
            out.retain(|s| !s.is_empty() && s != "C" && s != "POSIX");
            out
        })
        .unwrap_or_default();
    let mut plain = None;
    let mut best: Option<(usize, String)> = None;
    let mut rest = xml;
    while let Some(open) = rest.find("<comment") {
        rest = &rest[open + "<comment".len()..];
        let Some(close) = rest.find('>') else { break };
        let attrs = &rest[..close];
        let body_start = close + 1;
        let Some(end) = rest[body_start..].find("</comment>") else { break };
        let body = unescape(rest[body_start..body_start + end].trim());
        rest = &rest[body_start + end..];
        match attrs.split("xml:lang=\"").nth(1).and_then(|a| a.split('"').next()) {
            None => {
                plain.get_or_insert(body);
            }
            Some(l) => {
                if let Some(rank) = wanted.iter().position(|w| w == l)
                    && best.as_ref().is_none_or(|(r, _)| rank < *r)
                {
                    best = Some((rank, body));
                }
            }
        }
    }
    best.map(|(_, b)| b).or(plain).filter(|s| !s.is_empty())
}

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db(globs: &str, magic: &[u8]) -> MimeDb {
        let mut db = MimeDb::default();
        db.add_globs(globs.as_bytes());
        db.add_magic(magic);
        db.magic.sort_by_key(|s| std::cmp::Reverse(s.priority));
        db
    }

    const GLOBS: &str = "# comment\n50:application/pdf:*.pdf\n50:application/gzip:*.gz\n50:application/x-compressed-tar:*.tar.gz\n50:text/x-c:*.c\n50:text/x-c++src:*.C:cs\n50:text/x-makefile:makefile\n10:text/x-readme:readme*\n50:application/x-foo:*.amb\n50:application/x-bar:*.amb\n";

    #[test]
    fn names_decide_by_weight_then_the_longest_pattern() {
        let d = db(GLOBS, b"");
        assert_eq!(d.guess(b"Report.PDF", None), "application/pdf");
        assert_eq!(d.guess(b"x.tar.gz", None), "application/x-compressed-tar");
        assert_eq!(d.guess(b"x.gz", None), "application/gzip");
        assert_eq!(d.guess(b"Makefile", None), "text/x-makefile");
        assert_eq!(d.guess(b"README.md", None), "text/x-readme");
        // Case-sensitive patterns only match their own case.
        assert_eq!(d.guess(b"main.C", None), "text/x-c++src");
        assert_eq!(d.guess(b"main.c", None), "text/x-c");
    }

    fn magic_line(indent: u32, start: usize, value: &[u8], extra: &[u8]) -> Vec<u8> {
        let mut out = if indent > 0 { indent.to_string().into_bytes() } else { Vec::new() };
        out.extend_from_slice(format!(">{start}=").as_bytes());
        out.extend_from_slice(&(value.len() as u16).to_be_bytes());
        out.extend_from_slice(value);
        out.extend_from_slice(extra);
        out.push(b'\n');
        out
    }

    fn magic() -> Vec<u8> {
        let mut m = b"MIME-Magic\0\n".to_vec();
        m.extend_from_slice(b"[50:application/pdf]\n");
        m.extend(magic_line(0, 0, b"%PDF-", b"+1024"));
        m.extend_from_slice(b"[60:application/x-foo]\n");
        m.extend(magic_line(0, 0, b"AB", b""));
        m.extend(magic_line(1, 4, b"FOO", b""));
        m.extend_from_slice(b"[40:application/x-bar]\n");
        m.extend(magic_line(0, 0, b"AB", b""));
        m.extend_from_slice(b"[50:image/x-masked]\n");
        m.extend(magic_line(0, 0, b"\xf0\x0f", b"&\xf0\x0f"));
        m
    }

    #[test]
    fn contents_decide_when_the_name_cant() {
        let d = db(GLOBS, &magic());
        assert_eq!(d.guess(b"no-extension", Some(b"%PDF-1.7\n")), "application/pdf");
        // Inside the range.
        assert_eq!(d.guess(b"no-extension", Some(b"junk junk %PDF-1.7")), "application/pdf");
        // Two types share a name, and the bytes pick: the child test makes foo match.
        assert_eq!(d.guess(b"x.amb", Some(b"AB..FOO")), "application/x-foo");
        assert_eq!(d.guess(b"x.amb", Some(b"AB..BAR")), "application/x-bar");
        // Masks.
        assert_eq!(d.guess(b"blob", Some(b"\xff\xff")), "image/x-masked");
    }

    #[test]
    fn unknown_files_are_text_or_bytes() {
        let d = db(GLOBS, &magic());
        assert_eq!(d.guess(b"notes", Some(b"just words\n")), PLAIN_TEXT);
        assert_eq!(d.guess(b"notes", Some("naïve".as_bytes())), PLAIN_TEXT);
        assert_eq!(d.guess(b"blob", Some(b"\x00\x01\x02")), OCTET_STREAM);
        assert_eq!(d.guess(b"blob", None), OCTET_STREAM);
        assert_eq!(d.guess(b"empty", Some(b"")), EMPTY);
        // A multi-byte character cut off by the end of what was read is still text.
        assert_eq!(d.guess(b"notes", Some(&"ééé".as_bytes()[..5])), PLAIN_TEXT);
    }

    #[test]
    fn a_broken_magic_file_keeps_what_came_before() {
        let mut m = magic();
        m.extend_from_slice(b"[50:text/x-late]\n>0=\xff");
        let d = db("", &m);
        assert_eq!(d.guess(b"x", Some(b"%PDF-")), "application/pdf");
        assert!(db("", b"not magic at all").magic.is_empty());
    }

    #[test]
    fn word_sized_values_match_this_machines_byte_order() {
        let mut m = b"MIME-Magic\0\n[50:application/x-word]\n".to_vec();
        m.extend(magic_line(0, 0, &[0x12, 0x34], b"~2"));
        let d = db("", &m);
        assert_eq!(d.guess(b"x", Some(&0x1234u16.to_ne_bytes())), "application/x-word");
    }

    #[test]
    fn globs() {
        assert!(glob_match(b"*.tar.*", b"a.tar.gz"));
        assert!(glob_match(b"[Mm]akefile", b"Makefile"));
        assert!(glob_match(b"x?y", b"xay"));
        assert!(!glob_match(b"x?y", b"xy"));
        assert!(glob_match(b"[!a]b", b"cb"));
        assert!(!glob_match(b"[!a]b", b"ab"));
        assert!(glob_match(b"[a-c]x", b"bx"));
        assert!(!glob_match(b"*.pdf", b"pdf"));
        assert!(glob_match(b"*", b""));
    }

    #[test]
    fn descriptions_come_in_the_users_language() {
        let xml = r#"<mime-type type="application/pdf"><comment>PDF document</comment><comment xml:lang="de">PDF-Dokument</comment><comment xml:lang="pt_BR">Documento PDF</comment><comment xml:lang="zh-Hant-TW">PDF 文件</comment><comment xml:lang="x">A &amp; B</comment></mime-type>"#;
        assert_eq!(comment(xml, None).as_deref(), Some("PDF document"));
        assert_eq!(comment(xml, Some("de_DE.UTF-8")).as_deref(), Some("PDF-Dokument"));
        assert_eq!(comment(xml, Some("pt_BR.UTF-8")).as_deref(), Some("Documento PDF"));
        assert_eq!(comment(xml, Some("fr_FR.UTF-8")).as_deref(), Some("PDF document"));
        assert_eq!(comment(xml, Some("C")).as_deref(), Some("PDF document"));
        assert_eq!(comment(xml, Some("x")).as_deref(), Some("A & B"));
    }

    #[test]
    fn describe_reads_the_types_file_and_refuses_odd_names() {
        let s = crate::testutil::Scratch::new();
        s.file("application/pdf.xml", b"<comment>PDF document</comment>");
        let d = MimeDb { dirs: vec![s.path().to_path_buf()], ..Default::default() };
        assert_eq!(d.describe("application/pdf", None).as_deref(), Some("PDF document"));
        assert_eq!(d.describe("application/missing", None), None);
        assert_eq!(d.describe("../etc/passwd", None), None);
        assert_eq!(d.describe("application/../pdf", None), None);
    }

    #[test]
    fn generic_icons_fall_back_to_the_media_type() {
        let mut d = MimeDb::default();
        d.generic_icons.insert("application/zip".into(), "package-x-generic".into());
        assert_eq!(d.generic_icon("application/zip"), "package-x-generic");
        assert_eq!(d.generic_icon("audio/flac"), "audio-x-generic");
    }
}
