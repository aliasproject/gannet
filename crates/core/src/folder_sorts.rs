//! The order each folder was last sorted in, kept between runs in
//! `$XDG_STATE_HOME/gannet/sorts`.
//!
//! A folder with no order of its own takes the nearest folder above it that has one, so sorting
//! a folder sorts everything inside it too. Downloads, Pictures and Videos start newest first,
//! as if that had been chosen for them. Choosing the order a folder would get anyway forgets its
//! own, so a later change above it reaches it again.
//!
//! The file is one folder per line: the key, the direction, then the path, with `\` and newlines
//! escaped so any path fits on one line. Lines are oldest first; past [`LIMIT`] the folders
//! sorted longest ago are dropped.

use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::listing::{SortKey, SortSpec};

/// The most folders remembered.
pub const LIMIT: usize = 1000;

const HEADER: &[u8] = b"# Gannet folder sorts, oldest first: key, direction, folder\n";

pub struct FolderSorts {
    /// Where they're kept, or `None` to keep them only while running.
    file: Option<PathBuf>,
    /// Folders that start newest first.
    newest_first: Vec<PathBuf>,
    /// Oldest first.
    saved: Vec<(PathBuf, SortSpec)>,
    /// The file's inode, modification time and length when it was last read, to notice another
    /// window's changes. Every save renames a new file into place, so the inode usually changes
    /// even when two saves land within one tick of the clock with the same length. An inode can
    /// be reused, so this is a best effort: a missed change shows at the next one.
    seen: Option<(u64, SystemTime, u64)>,
}

impl FolderSorts {
    /// Remembers sorts only while running, with no folders newest first.
    pub fn in_memory() -> Self {
        Self { file: None, newest_first: Vec::new(), saved: Vec::new(), seen: None }
    }

    pub fn new(file: PathBuf, newest_first: Vec<PathBuf>) -> Self {
        let mut s = Self { file: Some(file), newest_first, saved: Vec::new(), seen: None };
        s.refresh();
        s
    }

    /// The order to show `dir` in.
    pub fn for_dir(&mut self, dir: &Path) -> SortSpec {
        self.refresh();
        self.lookup(dir, true)
    }

    /// Remembers that `dir` was sorted in `spec`. Errors are ignored: the sort still applies,
    /// it just won't be there next time.
    pub fn set(&mut self, dir: &Path, spec: SortSpec) {
        // Held from reading to renaming, so two windows saving at once can't drop each other's.
        let _lock = self.lock();
        // Another window may have saved since this one last looked.
        self.refresh();
        self.saved.retain(|(p, _)| p != dir);
        if self.lookup(dir, true) != spec {
            self.saved.push((dir.to_path_buf(), spec));
            let over = self.saved.len().saturating_sub(LIMIT);
            self.saved.drain(..over);
        }
        self.save();
    }

    /// The nearest order at or above `dir`, starting at `dir` itself if `own`.
    fn lookup(&self, dir: &Path, own: bool) -> SortSpec {
        let start = if own { Some(dir) } else { dir.parent() };
        for d in start.into_iter().flat_map(Path::ancestors) {
            if let Some((_, s)) = self.saved.iter().rev().find(|(p, _)| p == d) {
                return *s;
            }
            if self.newest_first.iter().any(|p| p == d) {
                return SortSpec { key: SortKey::Modified, descending: true };
            }
        }
        SortSpec::default()
    }

    fn stamp(&self) -> Option<(u64, SystemTime, u64)> {
        let m = std::fs::metadata(self.file.as_ref()?).ok()?;
        Some((m.ino(), m.modified().ok()?, m.len()))
    }

    /// An exclusive lock on a file beside the sorts, released when it's dropped. `None` if it
    /// can't be had soon, in which case saving goes ahead without it: this runs on a keypress,
    /// and another window stuck holding it mustn't freeze this one.
    fn lock(&self) -> Option<std::fs::File> {
        let dir = self.file.as_ref()?.parent()?;
        let _ = std::fs::create_dir_all(dir);
        let f = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(dir.join(".sorts.lock")).ok()?;
        let give_up = std::time::Instant::now() + std::time::Duration::from_secs(1);
        // SAFETY: the descriptor is open for as long as `f` lives.
        while unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            if e.kind() != std::io::ErrorKind::WouldBlock || std::time::Instant::now() >= give_up {
                return None;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        Some(f)
    }

    fn refresh(&mut self) {
        let Some(file) = &self.file else { return };
        let stamp = self.stamp();
        // Unchanged, or still missing: what's in memory is all there is.
        if stamp == self.seen {
            return;
        }
        match std::fs::read(file) {
            Ok(b) => self.saved = parse(&b),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => self.saved.clear(),
            // A read that fails for another reason says nothing about what's saved, so keep what's
            // known and try again next time; clearing it would make the next save forget the rest.
            Err(_) => return,
        }
        self.seen = stamp;
    }

    fn save(&mut self) {
        let Some(file) = &self.file else { return };
        let Some(dir) = file.parent() else { return };
        let _ = std::fs::create_dir_all(dir);
        // Written beside it and renamed over it, so a reader never sees half a file. Flushed to
        // the disk first, or a power cut after the rename could leave it empty.
        let tmp = dir.join(format!(".sorts.{}", std::process::id()));
        let ok = std::fs::File::create(&tmp).and_then(|mut f| f.write_all(&format(&self.saved)).and_then(|()| f.sync_all())).and_then(|()| std::fs::rename(&tmp, file));
        if ok.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        self.seen = self.stamp();
    }
}

fn key_word(k: SortKey) -> &'static str {
    match k {
        SortKey::Name => "name",
        SortKey::Size => "size",
        SortKey::Modified => "modified",
    }
}

fn format(saved: &[(PathBuf, SortSpec)]) -> Vec<u8> {
    let mut out = HEADER.to_vec();
    for (path, spec) in saved {
        out.extend_from_slice(key_word(spec.key).as_bytes());
        out.extend_from_slice(if spec.descending { b" desc " } else { b" asc " });
        for &b in path.as_os_str().as_bytes() {
            match b {
                b'\\' => out.extend_from_slice(b"\\\\"),
                b'\n' => out.extend_from_slice(b"\\n"),
                _ => out.push(b),
            }
        }
        out.push(b'\n');
    }
    out
}

/// Reads the file. Lines it doesn't understand, perhaps from a newer Gannet, are skipped.
fn parse(text: &[u8]) -> Vec<(PathBuf, SortSpec)> {
    let mut out: Vec<(PathBuf, SortSpec)> = Vec::new();
    for line in text.split(|&b| b == b'\n') {
        let mut parts = line.splitn(3, |&b| b == b' ');
        let (Some(key), Some(dir), Some(path)) = (parts.next(), parts.next(), parts.next()) else { continue };
        let key = match key {
            b"name" => SortKey::Name,
            b"size" => SortKey::Size,
            b"modified" => SortKey::Modified,
            _ => continue,
        };
        let descending = match dir {
            b"asc" => false,
            b"desc" => true,
            _ => continue,
        };
        let Some(path) = unescape(path) else { continue };
        if !path.is_absolute() {
            continue;
        }
        out.retain(|(p, _)| *p != path);
        out.push((path, SortSpec { key, descending }));
    }
    let over = out.len().saturating_sub(LIMIT);
    out.drain(..over);
    out
}

fn unescape(s: &[u8]) -> Option<PathBuf> {
    let mut out = Vec::with_capacity(s.len());
    let mut it = s.iter();
    while let Some(&b) = it.next() {
        if b == b'\\' {
            match it.next()? {
                b'\\' => out.push(b'\\'),
                b'n' => out.push(b'\n'),
                _ => return None,
            }
        } else {
            out.push(b);
        }
    }
    Some(PathBuf::from(std::ffi::OsString::from_vec(out)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::Scratch;

    const NAME: SortSpec = SortSpec { key: SortKey::Name, descending: false };
    const NEWEST: SortSpec = SortSpec { key: SortKey::Modified, descending: true };
    const BIGGEST: SortSpec = SortSpec { key: SortKey::Size, descending: true };

    #[test]
    fn folders_take_the_nearest_order_above_them() {
        let s = Scratch::new();
        let file = s.path().join("state/gannet/sorts");
        let mut f = FolderSorts::new(file.clone(), Vec::new());
        assert_eq!(f.for_dir(Path::new("/a/b")), NAME);
        f.set(Path::new("/a"), BIGGEST);
        assert_eq!(f.for_dir(Path::new("/a/b/c")), BIGGEST);
        f.set(Path::new("/a/b"), NAME);
        f.set(Path::new("/a"), NEWEST);
        assert_eq!(f.for_dir(Path::new("/a/b")), NAME);
        assert_eq!(f.for_dir(Path::new("/a/x")), NEWEST);
        // And again from the file, as the next run would.
        let mut again = FolderSorts::new(file, Vec::new());
        assert_eq!(again.for_dir(Path::new("/a/b/c")), NAME);
        assert_eq!(again.for_dir(Path::new("/a/x")), NEWEST);
    }

    #[test]
    fn choosing_what_a_folder_would_get_anyway_forgets_it() {
        let s = Scratch::new();
        let file = s.path().join("sorts");
        let mut f = FolderSorts::new(file.clone(), Vec::new());
        f.set(Path::new("/a"), BIGGEST);
        f.set(Path::new("/a/b"), NEWEST);
        f.set(Path::new("/a/b"), BIGGEST);
        f.set(Path::new("/c"), NAME);
        assert_eq!(f.saved, vec![(PathBuf::from("/a"), BIGGEST)]);
        f.set(Path::new("/a"), NEWEST);
        assert_eq!(f.for_dir(Path::new("/a/b")), NEWEST);
        assert_eq!(parse(&std::fs::read(file).unwrap()).len(), 1);
    }

    #[test]
    fn downloads_starts_newest_first_until_sorted() {
        let s = Scratch::new();
        let mut f = FolderSorts::new(s.path().join("sorts"), vec![PathBuf::from("/h/Downloads")]);
        assert_eq!(f.for_dir(Path::new("/h/Downloads")), NEWEST);
        assert_eq!(f.for_dir(Path::new("/h/Downloads/sub")), NEWEST);
        assert_eq!(f.for_dir(Path::new("/h/Documents")), NAME);
        // A sort chosen higher up doesn't reach past it.
        f.set(Path::new("/h"), BIGGEST);
        assert_eq!(f.for_dir(Path::new("/h/Downloads")), NEWEST);
        f.set(Path::new("/h/Downloads"), NAME);
        assert_eq!(f.for_dir(Path::new("/h/Downloads/sub")), NAME);
        f.set(Path::new("/h/Downloads"), NEWEST);
        assert_eq!(f.saved, vec![(PathBuf::from("/h"), BIGGEST)]);
    }

    #[test]
    fn odd_paths_round_trip() {
        let odd = PathBuf::from(std::ffi::OsString::from_vec(b"/x/new\nline \\back\\n \xff\xfe".to_vec()));
        let saved = vec![(odd.clone(), BIGGEST), (PathBuf::from("/plain"), NEWEST)];
        assert_eq!(parse(&format(&saved)), saved);
        assert_eq!(format(&saved).iter().filter(|&&b| b == b'\n').count(), 3);
    }

    #[test]
    fn unknown_lines_are_skipped() {
        let text = b"# note\nkind asc /a\nsize sideways /b\nsize desc relative\nsize desc /bad\\q\nmodified desc /ok\n\n";
        assert_eq!(parse(text), vec![(PathBuf::from("/ok"), NEWEST)]);
    }

    #[test]
    fn the_folders_sorted_longest_ago_go_first() {
        let mut f = FolderSorts::in_memory();
        for i in 0..LIMIT + 5 {
            f.set(&PathBuf::from(format!("/d{i}")), BIGGEST);
        }
        f.set(Path::new("/d5"), NEWEST);
        assert_eq!(f.saved.len(), LIMIT);
        assert_eq!(f.for_dir(Path::new("/d4")), NAME);
        assert_eq!(f.for_dir(Path::new("/d6")), BIGGEST);
        assert_eq!(f.saved.last().unwrap(), &(PathBuf::from("/d5"), NEWEST));
    }

    #[test]
    fn another_windows_change_is_seen() {
        let s = Scratch::new();
        let file = s.path().join("sorts");
        let mut one = FolderSorts::new(file.clone(), Vec::new());
        let mut two = FolderSorts::new(file, Vec::new());
        one.set(Path::new("/a"), BIGGEST);
        assert_eq!(two.for_dir(Path::new("/a")), BIGGEST);
        // Saving keeps the other window's folders.
        two.set(Path::new("/b"), NEWEST);
        assert_eq!(one.for_dir(Path::new("/a")), BIGGEST);
        assert_eq!(one.for_dir(Path::new("/b")), NEWEST);
    }

    #[test]
    fn a_failed_read_keeps_what_was_saved() {
        let s = Scratch::new();
        let file = s.path().join("sorts");
        let mut f = FolderSorts::new(file.clone(), Vec::new());
        f.set(Path::new("/a"), BIGGEST);
        // Something that can't be read where the file was: reading it fails, but not as missing.
        std::fs::remove_file(&file).unwrap();
        std::fs::create_dir(&file).unwrap();
        assert_eq!(f.for_dir(Path::new("/a")), BIGGEST);
        // Once it reads again, the file is what counts.
        std::fs::remove_dir(&file).unwrap();
        std::fs::write(&file, format(&[(PathBuf::from("/a"), NEWEST)])).unwrap();
        assert_eq!(f.for_dir(Path::new("/a")), NEWEST);
    }

    #[test]
    fn windows_saving_at_once_keep_each_others_folders() {
        let s = Scratch::new();
        let file = s.path().join("sorts");
        let threads: Vec<_> = (0..2)
            .map(|t| {
                let file = file.clone();
                std::thread::spawn(move || {
                    let mut f = FolderSorts::new(file, Vec::new());
                    for i in 0..60 {
                        f.set(&PathBuf::from(format!("/t{t}/d{i}")), BIGGEST);
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(parse(&std::fs::read(&file).unwrap()).len(), 120);
    }

    #[test]
    fn an_unwritable_file_still_sorts_while_running() {
        let s = Scratch::new();
        s.file("blocker", b"");
        let mut f = FolderSorts::new(s.path().join("blocker/sorts"), Vec::new());
        f.set(Path::new("/a"), BIGGEST);
        assert_eq!(f.for_dir(Path::new("/a")), BIGGEST);
    }
}
