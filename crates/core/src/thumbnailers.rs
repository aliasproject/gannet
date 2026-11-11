//! The system's thumbnailers: the `.thumbnailer` files other programs install so file managers can
//! get a picture of a file they can't read themselves, like a HEIC photo, an SVG or an office
//! document. Each names the types it handles and a command line for making a picture.
//!
//! Folders are read most important first, and the first entry for a type wins. Running one is the
//! caller's business: here they're only found and their command lines filled in.

use std::collections::HashMap;
use std::ffi::OsString;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// Bigger than any real entry, so a file that isn't one costs nothing to turn away.
const MAX_ENTRY: u64 = 64 * 1024;

#[derive(Clone, Debug, PartialEq)]
pub struct Thumbnailer {
    /// The command line, before its `%` codes are filled in.
    pub exec: Vec<String>,
    pub try_exec: Option<String>,
}

#[derive(Debug, Default)]
pub struct Thumbnailers {
    by_mime: HashMap<String, usize>,
    list: Vec<Thumbnailer>,
}

impl Thumbnailers {
    /// Reads every `*.thumbnailer` in `dirs` (each a `thumbnailers` folder), the first folder
    /// winning, and within a folder the first by name.
    pub fn load(dirs: &[PathBuf]) -> Self {
        let mut out = Self::default();
        for dir in dirs {
            let Ok(entries) = std::fs::read_dir(dir) else { continue };
            let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "thumbnailer")).collect();
            paths.sort();
            for path in paths {
                let Ok(meta) = std::fs::metadata(&path) else { continue };
                if !meta.is_file() || meta.len() > MAX_ENTRY {
                    continue;
                }
                let Ok(text) = std::fs::read_to_string(&path) else { continue };
                out.add(&text);
            }
        }
        out
    }

    /// Adds one entry, unless it's missing something it needs.
    pub fn add(&mut self, text: &str) {
        let Some((thumbnailer, mimes)) = parse(text) else { return };
        let index = self.list.len();
        let mut used = false;
        for mime in mimes {
            if let std::collections::hash_map::Entry::Vacant(slot) = self.by_mime.entry(mime) {
                slot.insert(index);
                used = true;
            }
        }
        if used {
            self.list.push(thumbnailer);
        }
    }

    pub fn find(&self, mime: &str) -> Option<&Thumbnailer> {
        self.list.get(*self.by_mime.get(mime)?)
    }
}

/// The entry's command, and the types it's for.
fn parse(text: &str) -> Option<(Thumbnailer, Vec<String>)> {
    let mut in_entry = false;
    let (mut exec, mut try_exec, mut mimes) = (None, None, Vec::new());
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_entry = line == "[Thumbnailer Entry]";
            continue;
        }
        if !in_entry || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else { continue };
        match key.trim() {
            "Exec" => exec = split_exec(value.trim()),
            "TryExec" => try_exec = Some(value.trim().to_string()).filter(|v| !v.is_empty()),
            "MimeType" => mimes = value.split(';').map(str::trim).filter(|m| !m.is_empty()).map(str::to_string).collect(),
            _ => {}
        }
    }
    let exec = exec.filter(|e: &Vec<String>| !e.is_empty())?;
    (!mimes.is_empty()).then_some((Thumbnailer { exec, try_exec }, mimes))
}

/// An `Exec` line as words, as the desktop entry spec quotes them: double quotes around a word
/// that has spaces, with a backslash before `"`, `` ` ``, `$` and `\` inside. `None` if the
/// quoting doesn't end.
fn split_exec(line: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut word = String::new();
    let (mut quoted, mut started) = (false, false);
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                quoted = !quoted;
                started = true;
            }
            '\\' if quoted => word.push(chars.next()?),
            ' ' | '\t' if !quoted => {
                if started || !word.is_empty() {
                    words.push(std::mem::take(&mut word));
                }
                started = false;
            }
            c => word.push(c),
        }
    }
    if quoted {
        return None;
    }
    if started || !word.is_empty() {
        words.push(word);
    }
    Some(words)
}

/// `file://` and the path, with anything that isn't plain escaped, as `%u` wants.
pub fn file_uri(path: &Path) -> String {
    let mut out = String::from("file://");
    for &b in path.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || b"/-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

impl Thumbnailer {
    /// The program to run, found in `programs` (folders, in order) when it's named without a
    /// path. `None` if it isn't in one of them, which also means it can't be seen from a jail
    /// that has only those folders.
    pub fn program(&self, programs: &[PathBuf]) -> Option<PathBuf> {
        let name = Path::new(&self.exec[0]);
        let found = if name.is_absolute() { programs.iter().any(|dir| name.starts_with(dir)).then(|| name.to_path_buf()) } else if name.components().count() == 1 { programs.iter().map(|dir| dir.join(name)).find(|p| p.is_file()) } else { None };
        found.filter(|p| p.is_file())
    }

    /// The command line for a picture of `input` at most `size` pixels across, written to
    /// `output`, with `program` (from `program`) in front. `None` for a code it doesn't know.
    pub fn command(&self, program: &Path, input: &Path, output: &Path, size: u32) -> Option<Vec<OsString>> {
        let mut out: Vec<OsString> = vec![program.as_os_str().to_owned()];
        for word in &self.exec[1..] {
            // A word that's only a code can be any bytes, as a path can.
            match word.as_str() {
                "%i" => out.push(input.as_os_str().to_owned()),
                "%o" => out.push(output.as_os_str().to_owned()),
                "%u" => out.push(file_uri(input).into()),
                _ => {
                    let mut filled = String::new();
                    let mut chars = word.chars();
                    while let Some(c) = chars.next() {
                        if c != '%' {
                            filled.push(c);
                            continue;
                        }
                        match chars.next()? {
                            's' => filled.push_str(&size.to_string()),
                            'u' => filled.push_str(&file_uri(input)),
                            'i' => filled.push_str(input.to_str()?),
                            'o' => filled.push_str(output.to_str()?),
                            '%' => filled.push('%'),
                            _ => return None,
                        }
                    }
                    out.push(filled.into());
                }
            }
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::Scratch;

    const OFFICE: &str = "[Thumbnailer Entry]\nTryExec=gsf-office-thumbnailer\nExec=gsf-office-thumbnailer -i %i -o %o -s %s\nMimeType=application/msword;application/vnd.oasis.opendocument.text;\n";

    #[test]
    fn entries_are_read_with_the_first_winning() {
        let s = Scratch::new();
        s.file("user/thumbnailers/mine.thumbnailer", b"[Thumbnailer Entry]\nExec=my-thumbs %u %o\nMimeType=image/svg+xml;\n");
        s.file("sys/thumbnailers/b.thumbnailer", b"[Thumbnailer Entry]\nExec=glycin --input %u --output %o --size %s\nMimeType=image/svg+xml;image/jxl\n");
        s.file("sys/thumbnailers/a.thumbnailer", OFFICE.as_bytes());
        s.file("sys/thumbnailers/broken.thumbnailer", b"[Thumbnailer Entry]\nMimeType=text/x-nothing;\n");
        s.file("sys/thumbnailers/notes.txt", b"[Thumbnailer Entry]\nExec=nope\nMimeType=image/png;\n");
        let t = Thumbnailers::load(&[s.path().join("user/thumbnailers"), s.path().join("missing"), s.path().join("sys/thumbnailers")]);
        assert_eq!(t.find("image/svg+xml").unwrap().exec[0], "my-thumbs", "the user's own comes first");
        assert_eq!(t.find("image/jxl").unwrap().exec[0], "glycin");
        assert_eq!(t.find("application/msword").unwrap().try_exec.as_deref(), Some("gsf-office-thumbnailer"));
        assert!(t.find("text/x-nothing").is_none(), "no command");
        assert!(t.find("image/png").is_none(), "not a .thumbnailer file");
    }

    #[test]
    fn command_lines_are_filled_in() {
        let mut t = Thumbnailers::default();
        t.add(OFFICE);
        t.add("[Thumbnailer Entry]\nExec=thumb \"--size=%s\" --uri=%u \"a \\\"quoted\\\" word\" 100%%\nMimeType=image/x-odd;\n");
        let office = t.find("application/msword").unwrap();
        let line = office.command(Path::new("/usr/bin/gsf-office-thumbnailer"), Path::new("/in/my file.doc"), Path::new("/out/t.png"), 512).unwrap();
        assert_eq!(line, ["/usr/bin/gsf-office-thumbnailer", "-i", "/in/my file.doc", "-o", "/out/t.png", "-s", "512"].map(OsString::from));
        let odd = t.find("image/x-odd").unwrap();
        let line = odd.command(Path::new("/usr/bin/thumb"), Path::new("/in/é x"), Path::new("/o"), 64).unwrap();
        assert_eq!(line, ["/usr/bin/thumb", "--size=64", "--uri=file:///in/%C3%A9%20x", "a \"quoted\" word", "100%"].map(OsString::from));
        t.add("[Thumbnailer Entry]\nExec=bad %z\nMimeType=image/x-bad;\n");
        assert!(t.find("image/x-bad").unwrap().command(Path::new("/usr/bin/bad"), Path::new("/i"), Path::new("/o"), 1).is_none(), "an unknown code");
        assert!(split_exec("never \"ends").is_none());
    }

    #[test]
    fn programs_are_only_found_where_asked() {
        let s = Scratch::new();
        let bin = s.dir("bin");
        s.file("bin/thumb", b"");
        let mut t = Thumbnailers::default();
        t.add("[Thumbnailer Entry]\nExec=thumb %o\nMimeType=a/a;\n");
        t.add("[Thumbnailer Entry]\nExec=/opt/elsewhere/thumb %o\nMimeType=b/b;\n");
        t.add("[Thumbnailer Entry]\nExec=../bin/thumb %o\nMimeType=c/c;\n");
        t.add("[Thumbnailer Entry]\nExec=missing %o\nMimeType=d/d;\n");
        let dirs = [bin.clone()];
        assert_eq!(t.find("a/a").unwrap().program(&dirs), Some(bin.join("thumb")));
        assert_eq!(t.find("b/b").unwrap().program(&dirs), None, "outside the folders the jail has");
        assert_eq!(t.find("c/c").unwrap().program(&dirs), None, "a relative path");
        assert_eq!(t.find("d/d").unwrap().program(&dirs), None);
    }
}
