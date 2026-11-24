//! Folders pinned to the sidebar. They live in GTK's bookmarks file, one URI per line with an
//! optional label after a space, so Nautilus and GTK's Open and Save dialogs share them. The
//! line order is the pin order. Gannet shows only local folders, and every other line, such as
//! an `sftp://` bookmark, stays exactly as it was when the file is written again. The one
//! exception is a folder named on more than one line: moving or unpinning it takes all of them,
//! as only the first one counts.

use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use crate::uri::{from_file_uri, to_file_uri};
use crate::watch::Watch;

/// Where the file is, under `$XDG_CONFIG_HOME`.
pub const FILE: &str = "gtk-3.0/bookmarks";

#[derive(Clone, Debug, PartialEq)]
pub struct Pin {
    pub path: PathBuf,
    /// The name it was given, if any. Otherwise it goes by its folder's name.
    pub label: Option<String>,
}

impl Pin {
    pub fn name(&self) -> String {
        match (&self.label, self.path.file_name()) {
            (Some(l), _) => l.clone(),
            (None, Some(n)) => n.to_string_lossy().into_owned(),
            (None, None) => self.path.to_string_lossy().into_owned(),
        }
    }
}

/// Where pins go: in front of a pin, or after the last one. A pin rather than a number, so that
/// another program changing the file meanwhile can't make it land somewhere else.
#[derive(Clone, Debug, PartialEq)]
pub enum Spot {
    Before(PathBuf),
    End,
}

/// The local folder a line names, if it names one, with any trailing `/` gone.
fn line_path(line: &[u8]) -> Option<PathBuf> {
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    let uri = line.split(|&b| b == b' ').next()?;
    let path = from_file_uri(std::str::from_utf8(uri).ok()?)?;
    path.is_absolute().then(|| path.components().collect())
}

fn line_label(line: &[u8]) -> Option<String> {
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    let at = line.iter().position(|&b| b == b' ')?;
    let label = String::from_utf8_lossy(&line[at + 1..]).trim().to_string();
    (!label.is_empty()).then_some(label)
}

fn lines(text: &[u8]) -> Vec<&[u8]> {
    // A line keeps a `\r` it ends in, so a file written with CRLF stays that way.
    let mut out: Vec<&[u8]> = text.split(|&b| b == b'\n').collect();
    // The newline that ends the file doesn't start another line.
    if out.last().is_some_and(|l| l.is_empty()) {
        out.pop();
    }
    out
}

/// The pins in the file, in order. A folder named twice is pinned once, where it first appears.
pub fn parse(text: &[u8]) -> Vec<Pin> {
    let mut out: Vec<Pin> = Vec::new();
    for line in lines(text) {
        if let Some(path) = line_path(line)
            && !out.iter().any(|p| p.path == path)
        {
            out.push(Pin { path, label: line_label(line) });
        }
    }
    out
}

/// The pins in the file at `file`. With no file, there are none.
pub fn read(file: &Path) -> Vec<Pin> {
    parse(&std::fs::read(file).unwrap_or_default())
}

fn join(lines: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    for l in lines {
        out.extend_from_slice(l);
        out.push(b'\n');
    }
    out
}

/// Puts `new` lines at `at`: in front of that pin's line, or after the last pin's line, or at
/// the end if there's no pin in the file. A pin that's gone counts as the end.
fn insert(lines: &mut Vec<Vec<u8>>, new: Vec<Vec<u8>>, at: &Spot) {
    let before = match at {
        Spot::Before(p) => lines.iter().position(|l| line_path(l).as_ref() == Some(p)),
        Spot::End => None,
    };
    let i = before.unwrap_or_else(|| lines.iter().rposition(|l| line_path(l).is_some()).map_or(lines.len(), |i| i + 1));
    lines.splice(i..i, new);
}

/// The file with these folders pinned at `at`, in order. Ones already pinned stay where they
/// are. `None` if nothing changes.
pub fn with_added(text: &[u8], paths: &[PathBuf], at: &Spot) -> Option<Vec<u8>> {
    let pinned: Vec<PathBuf> = parse(text).into_iter().map(|p| p.path).collect();
    let mut new: Vec<PathBuf> = Vec::new();
    for p in paths {
        let p: PathBuf = p.components().collect();
        if !pinned.contains(&p) && !new.contains(&p) {
            new.push(p);
        }
    }
    if new.is_empty() {
        return None;
    }
    let mut all: Vec<Vec<u8>> = lines(text).into_iter().map(<[u8]>::to_vec).collect();
    insert(&mut all, new.iter().map(|p| to_file_uri(p).into_bytes()).collect(), at);
    Some(join(&all))
}

/// The file with `path` unpinned, wherever it's named. `None` if it wasn't pinned.
pub fn without(text: &[u8], path: &Path) -> Option<Vec<u8>> {
    let all = lines(text);
    let kept: Vec<Vec<u8>> = all.iter().filter(|l| line_path(l).as_deref() != Some(path)).map(|l| l.to_vec()).collect();
    (kept.len() != all.len()).then(|| join(&kept))
}

/// The file with the pin for `path` moved to `at`, keeping its label. `None` if it isn't pinned
/// any more, or it's already there.
pub fn with_moved(text: &[u8], path: &Path, at: &Spot) -> Option<Vec<u8>> {
    if *at == Spot::Before(path.to_path_buf()) {
        return None;
    }
    let all = lines(text);
    let first = all.iter().position(|l| line_path(l).as_deref() == Some(path))?;
    let line = all[first].to_vec();
    let mut kept: Vec<Vec<u8>> = all.iter().filter(|l| line_path(l).as_deref() != Some(path)).map(|l| l.to_vec()).collect();
    insert(&mut kept, vec![line], at);
    let out = join(&kept);
    (parse(&out) != parse(text)).then_some(out)
}

/// Reads the file, changes it, and writes it back if `change` says what to. Returns whether it
/// was written. Reading right before writing keeps what other programs wrote meanwhile.
pub fn edit(file: &Path, change: impl FnOnce(&[u8]) -> Option<Vec<u8>>) -> std::io::Result<bool> {
    let text = match std::fs::read(file) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e),
    };
    let Some(new) = change(&text) else { return Ok(false) };
    save(file, &new)?;
    Ok(true)
}

/// Writes the file whole: into a new file beside it, then renamed over it, so another program
/// never reads half of it. A file that's a link to another, as dotfile managers make, has the
/// one it points at replaced, and stays a link.
pub fn save(file: &Path, text: &[u8]) -> std::io::Result<()> {
    let file = match std::fs::symlink_metadata(file) {
        Ok(m) if m.file_type().is_symlink() => std::fs::canonicalize(file)?,
        _ => file.to_path_buf(),
    };
    let dir = file.parent().ok_or_else(|| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    std::fs::create_dir_all(dir)?;
    let mut name = b".".to_vec();
    name.extend_from_slice(file.file_name().map_or(&b"bookmarks"[..], |n| n.as_bytes()));
    name.extend_from_slice(format!(".gannet-{}", std::process::id()).as_bytes());
    let temp = dir.join(std::ffi::OsStr::from_bytes(&name));
    let mode = std::fs::metadata(&file).ok().map(|m| m.permissions());
    let written = (|| {
        let mut f = std::fs::File::create(&temp)?;
        // The file keeps the permissions it had.
        if let Some(mode) = mode {
            f.set_permissions(mode)?;
        }
        f.write_all(text)?;
        f.sync_all()?;
        std::fs::rename(&temp, &file)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    written
}

/// Notices the file changing, whoever changes it. It watches the folder the file is in, as
/// programs replace the file rather than write into it, or `$XDG_CONFIG_HOME` until that
/// folder exists.
pub struct BookmarksWatch {
    pub watch: Watch,
    config_home: PathBuf,
}

impl BookmarksWatch {
    pub fn new(config_home: PathBuf) -> rustix::io::Result<Self> {
        let w = Self { watch: Watch::new()?, config_home };
        w.rearm();
        Ok(w)
    }

    fn dir(&self) -> PathBuf {
        let gtk = self.config_home.join("gtk-3.0");
        if gtk.is_dir() { gtk } else { self.config_home.clone() }
    }

    fn rearm(&self) {
        // Set, then looked at again: the folder can appear or go in between.
        for _ in 0..4 {
            let dir = self.dir();
            self.watch.set_dirs(std::slice::from_ref(&dir));
            if self.dir() == dir {
                break;
            }
        }
    }

    /// Call when the watch's fd is readable. True if the file should be read again.
    pub fn changed(&mut self) -> bool {
        let any = self.watch.drain();
        if any {
            self.rearm();
        }
        any
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::Scratch;

    const SAMPLE: &[u8] = b"file:///home/me/Projects\nsftp://server/srv Server\nfile:///home/me/My%20Music Tunes\nfile:///home/me/Projects/\n\nfile:///home/me/Notes\n";

    fn paths(text: &[u8]) -> Vec<String> {
        parse(text).into_iter().map(|p| p.path.to_string_lossy().into_owned()).collect()
    }

    #[test]
    fn only_local_folders_are_pins_each_once_with_their_labels() {
        let pins = parse(SAMPLE);
        assert_eq!(paths(SAMPLE), ["/home/me/Projects", "/home/me/My Music", "/home/me/Notes"]);
        assert_eq!(pins[1].label.as_deref(), Some("Tunes"));
        assert_eq!(pins.iter().map(Pin::name).collect::<Vec<_>>(), ["Projects", "Tunes", "Notes"]);
        assert_eq!(parse(b"file://localhost/srv/x\r\nfile:relative\n  \nnot a uri\n"), [Pin { path: "/srv/x".into(), label: None }]);
    }

    #[test]
    fn adding_keeps_every_other_line_as_it_was() {
        let out = with_added(SAMPLE, &["/home/me/Work Stuff".into()], &Spot::Before("/home/me/My Music".into())).unwrap();
        assert_eq!(out, b"file:///home/me/Projects\nsftp://server/srv Server\nfile:///home/me/Work%20Stuff\nfile:///home/me/My%20Music Tunes\nfile:///home/me/Projects/\n\nfile:///home/me/Notes\n");
        // At the end, after the last pin, and a file with no newline at its end gets one.
        let out = with_added(b"file:///a\nsmb://host/share", &["/b".into()], &Spot::End).unwrap();
        assert_eq!(out, b"file:///a\nfile:///b\nsmb://host/share\n");
        assert_eq!(with_added(b"", &["/b".into(), "/c".into()], &Spot::End).unwrap(), b"file:///b\nfile:///c\n");
    }

    #[test]
    fn crlf_lines_stay_crlf() {
        let out = with_added(b"file:///a\r\nsftp://h/x Server\r\n", &["/b".into()], &Spot::Before("/a".into())).unwrap();
        assert_eq!(out, b"file:///b\nfile:///a\r\nsftp://h/x Server\r\n");
        assert_eq!(parse(&out), [Pin { path: "/b".into(), label: None }, Pin { path: "/a".into(), label: None }]);
    }

    #[test]
    fn a_folder_is_only_pinned_once() {
        assert_eq!(with_added(SAMPLE, &["/home/me/Projects/".into()], &Spot::End), None);
        let out = with_added(SAMPLE, &["/home/me/Notes".into(), "/x".into(), "/x".into()], &Spot::End).unwrap();
        assert_eq!(paths(&out), ["/home/me/Projects", "/home/me/My Music", "/home/me/Notes", "/x"]);
    }

    #[test]
    fn a_spot_whose_pin_has_gone_is_the_end() {
        let out = with_added(SAMPLE, &["/x".into()], &Spot::Before("/gone".into())).unwrap();
        assert_eq!(paths(&out).last().map(String::as_str), Some("/x"));
    }

    #[test]
    fn unpinning_takes_every_line_for_that_folder_and_nothing_else() {
        let out = without(SAMPLE, Path::new("/home/me/Projects")).unwrap();
        assert_eq!(out, b"sftp://server/srv Server\nfile:///home/me/My%20Music Tunes\n\nfile:///home/me/Notes\n");
        assert_eq!(without(SAMPLE, Path::new("/home/me")), None);
    }

    #[test]
    fn moving_keeps_the_label_and_other_lines() {
        let out = with_moved(SAMPLE, Path::new("/home/me/My Music"), &Spot::Before("/home/me/Projects".into())).unwrap();
        assert_eq!(out, b"file:///home/me/My%20Music Tunes\nfile:///home/me/Projects\nsftp://server/srv Server\nfile:///home/me/Projects/\n\nfile:///home/me/Notes\n");
        let out = with_moved(SAMPLE, Path::new("/home/me/Projects"), &Spot::End).unwrap();
        assert_eq!(paths(&out), ["/home/me/My Music", "/home/me/Notes", "/home/me/Projects"]);
        // Where it already is, or gone: nothing to write.
        assert_eq!(with_moved(SAMPLE, Path::new("/home/me/Projects"), &Spot::Before("/home/me/My Music".into())), None);
        assert_eq!(with_moved(SAMPLE, Path::new("/home/me/Notes"), &Spot::End), None);
        assert_eq!(with_moved(SAMPLE, Path::new("/gone"), &Spot::End), None);
    }

    #[test]
    fn editing_reads_what_others_wrote_and_replaces_the_file_whole() {
        let s = Scratch::new();
        let file = s.path().join("config/gtk-3.0/bookmarks");
        assert!(read(&file).is_empty(), "no file, no pins");
        assert!(edit(&file, |t| with_added(t, &["/a".into()], &Spot::End)).unwrap());
        assert_eq!(std::fs::read(&file).unwrap(), b"file:///a\n");
        // Another program adds a line between Gannet's read and its next edit.
        std::fs::write(&file, b"file:///a\nsftp://h/x\n").unwrap();
        assert!(edit(&file, |t| with_added(t, &["/b".into()], &Spot::Before("/a".into()))).unwrap());
        assert_eq!(std::fs::read(&file).unwrap(), b"file:///b\nfile:///a\nsftp://h/x\n");
        assert!(!edit(&file, |t| without(t, Path::new("/nothing"))).unwrap(), "nothing to write");
        let left: Vec<_> = std::fs::read_dir(file.parent().unwrap()).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(left, ["bookmarks"], "no temporary file is left behind");
    }

    #[test]
    fn the_file_keeps_its_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let s = Scratch::new();
        let file = s.file("gtk-3.0/bookmarks", b"file:///a\n");
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(edit(&file, |t| with_added(t, &["/b".into()], &Spot::End)).unwrap());
        assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn a_linked_file_stays_a_link() {
        let s = Scratch::new();
        let real = s.file("dotfiles/bookmarks", b"file:///a\n");
        let file = s.path().join("config/gtk-3.0/bookmarks");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&real, &file).unwrap();
        assert!(edit(&file, |t| with_added(t, &["/b".into()], &Spot::End)).unwrap());
        assert!(std::fs::symlink_metadata(&file).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read(&real).unwrap(), b"file:///a\nfile:///b\n");
    }

    #[test]
    fn the_watch_sees_the_folder_appear_and_the_file_replaced() {
        let s = Scratch::new();
        let config = s.dir("config");
        let mut w = BookmarksWatch::new(config.clone()).unwrap();
        assert!(!w.changed());
        std::fs::create_dir(config.join("gtk-3.0")).unwrap();
        assert!(w.changed(), "the folder appearing");
        save(&config.join(FILE), b"file:///a\n").unwrap();
        assert!(w.changed(), "the file written");
        // Something unrelated in the config folder no longer counts once the folder's there.
        std::fs::write(config.join("other.conf"), b"x").unwrap();
        assert!(!w.changed());
        save(&config.join(FILE), b"file:///b\n").unwrap();
        assert!(w.changed(), "replaced again");
    }
}
