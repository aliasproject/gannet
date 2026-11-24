//! Finding names in a folder and every folder below it, by reading them as they are now. No
//! index is kept, so nothing runs while nobody is searching.
//!
//! The walk goes a level at a time, so what's near the top is found before what's deep, and
//! the first matches arrive while the rest is still being read. Like `find -xdev`, it stays on
//! the folder's own mount: another disk, a network share or `/proc` mounted inside isn't read,
//! and nor is anything a symlink leads to. Hidden folders are only read when hidden files show.

use std::collections::VecDeque;
use std::ffi::{CStr, CString};
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use rustix::fs::{AtFlags, FileType, Mode, OFlags, RawDir, Statx, StatxFlags, statat, statx};

use crate::listing::{Kind, name_contains};

/// The most matches a search lists. A query that finds more is too broad to be worth scrolling
/// through, and stopping there also stops the walk.
pub const LIMIT: usize = 1000;
/// Entries read between calls to `Step::Tick`, which is when a walk can be stopped.
const TICK_EVERY: u32 = 256;
const READ_BUFFER: usize = 32 * 1024;

/// One match.
pub struct Found {
    pub path: PathBuf,
    pub kind: Kind,
    /// `None` if it couldn't be looked at, as when it went in the meantime.
    pub size: Option<u64>,
    pub mtime: i64,
}

/// What the walker tells its caller as it goes.
pub enum Step {
    Found(Found),
    /// Some entries have gone by, or a folder was finished. Returning false stops the walk.
    Tick,
}

/// How a walk that ran to its end went.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Walked {
    /// It stopped at the limit, so there may be more.
    pub truncated: bool,
    /// Folders below the top one that couldn't be read, as when permission is denied.
    pub unreadable: usize,
}

/// Searches `root` and below for names containing `query`, ignoring case. `step` hears of
/// each match and is asked now and then whether to go on, and returning false from either
/// stops the walk; `None` means it was stopped.
/// Fails only if `root` itself can't be read.
pub fn walk(root: &Path, query: &str, show_hidden: bool, limit: usize, step: &mut dyn FnMut(Step) -> bool) -> std::io::Result<Option<Walked>> {
    let needle = query.to_lowercase();
    // The top folder can be reached through a symlink, as the folder showing can be.
    let top = open_dir(root, true)?;
    let mount = mount_of(&statx(top.as_fd(), c"", AtFlags::EMPTY_PATH, StatxFlags::MNT_ID).map_err(std::io::Error::from)?);
    let mut walked = Walked::default();
    let mut buf: Vec<MaybeUninit<u8>> = vec![MaybeUninit::uninit(); READ_BUFFER];
    let mut found = 0usize;
    let mut since_tick = 0u32;
    let mut queue: VecDeque<(PathBuf, Option<OwnedFd>)> = VecDeque::from([(root.to_path_buf(), Some(top))]);
    while let Some((dir, fd)) = queue.pop_front() {
        if !step(Step::Tick) {
            return Ok(None);
        }
        let fd = match fd {
            Some(fd) => fd,
            // A folder that became a symlink since it was listed isn't followed either.
            None => match open_dir(&dir, false) {
                Ok(fd) => fd,
                // One that went since it was listed was never there to read.
                Err(e) if matches!(e.kind(), std::io::ErrorKind::NotFound) => continue,
                Err(_) => {
                    walked.unreadable += 1;
                    continue;
                }
            },
        };
        let mut entries = RawDir::new(fd.as_fd(), &mut buf);
        while let Some(e) = entries.next() {
            let Ok(e) = e else {
                walked.unreadable += 1;
                break;
            };
            since_tick += 1;
            if since_tick >= TICK_EVERY {
                since_tick = 0;
                if !step(Step::Tick) {
                    return Ok(None);
                }
            }
            let name = e.file_name();
            let b = name.to_bytes();
            if b == b"." || b == b".." || (!show_hidden && b.first() == Some(&b'.')) {
                continue;
            }
            let mut ty = e.file_type();
            if name_contains(b, &needle) {
                let at = dir.join(std::ffi::OsStr::from_bytes(b));
                let st = statx(fd.as_fd(), name, AtFlags::SYMLINK_NOFOLLOW | AtFlags::NO_AUTOMOUNT, StatxFlags::TYPE | StatxFlags::SIZE | StatxFlags::MTIME).ok();
                if let Some(st) = &st {
                    ty = FileType::from_raw_mode(st.stx_mode as _);
                }
                let kind = match ty {
                    FileType::RegularFile => Kind::File,
                    FileType::Directory => Kind::Dir,
                    FileType::Symlink => match statat(fd.as_fd(), name, AtFlags::empty()) {
                        Ok(t) if FileType::from_raw_mode(t.st_mode as _) == FileType::Directory => Kind::LinkDir,
                        _ => Kind::Link,
                    },
                    _ => Kind::Other,
                };
                // Looking at a match can be slow, so a search that's been replaced stops here too.
                if !step(Step::Found(Found { path: at, kind, size: st.as_ref().map(|s| s.stx_size), mtime: st.map_or(0, |s| s.stx_mtime.tv_sec) })) {
                    return Ok(None);
                }
                found += 1;
                if found >= limit {
                    walked.truncated = true;
                    return Ok(Some(walked));
                }
            }
            if matches!(ty, FileType::Directory | FileType::Unknown) && same_mount(&fd, name, mount) {
                queue.push_back((dir.join(std::ffi::OsStr::from_bytes(b)), None));
            }
        }
    }
    Ok(Some(walked))
}

/// Opens a folder to read. Neither waits on a FIFO nor, unless asked, follows a symlink.
fn open_dir(path: &Path, follow: bool) -> std::io::Result<OwnedFd> {
    let mut flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NONBLOCK | OFlags::CLOEXEC;
    if !follow {
        flags |= OFlags::NOFOLLOW;
    }
    let c = CString::new(path.as_os_str().as_bytes()).map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    Ok(rustix::fs::open(c.as_c_str(), flags, Mode::empty())?)
}

/// Which mount something is on. Kernels before 5.8 don't say, so the device stands in, which
/// also tells another disk apart.
fn mount_of(st: &Statx) -> (bool, u64) {
    if st.stx_mask & StatxFlags::MNT_ID.bits() != 0 { (true, st.stx_mnt_id) } else { (false, (u64::from(st.stx_dev_major) << 32) | u64::from(st.stx_dev_minor)) }
}

/// Whether `name` in `dir` is a folder on the walk's own mount. A mount point is the root of
/// another mount, and with no automount an automounted folder doesn't get mounted by asking.
fn same_mount(dir: &OwnedFd, name: &CStr, mount: (bool, u64)) -> bool {
    match statx(dir.as_fd(), name, AtFlags::SYMLINK_NOFOLLOW | AtFlags::NO_AUTOMOUNT, StatxFlags::TYPE | StatxFlags::MNT_ID) {
        Ok(st) => FileType::from_raw_mode(st.stx_mode as _) == FileType::Directory && mount_of(&st) == mount,
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::Scratch;
    use std::os::unix::fs::symlink;

    /// Every match's path below `root`, in the order found, and how the walk went.
    fn search(root: &Path, query: &str, hidden: bool, limit: usize) -> (Vec<String>, Walked) {
        let mut out = Vec::new();
        let walked = walk(root, query, hidden, limit, &mut |s| {
            if let Step::Found(f) = s {
                out.push(f.path.strip_prefix(root).unwrap().to_string_lossy().into_owned());
            }
            true
        })
        .unwrap()
        .unwrap();
        (out, walked)
    }

    #[test]
    fn finds_names_at_every_depth_ignoring_case_and_nearer_ones_first() {
        let s = Scratch::new();
        s.file("a/b/c/Deep-Report.txt", b"x");
        s.file("a/report.md", b"xy");
        s.file("REPORTS/other", b"");
        s.file("notes.txt", b"");
        let (found, walked) = search(s.path(), "report", false, LIMIT);
        assert_eq!(walked, Walked::default());
        // One level at a time: the folder first, then a/, then a/b/c.
        assert_eq!(found[0], "REPORTS");
        assert_eq!(found[1..], ["a/report.md", "a/b/c/Deep-Report.txt"]);
    }

    #[test]
    fn a_match_says_what_it_is_and_its_size_and_date() {
        let s = Scratch::new();
        s.file("dir/match.bin", &[0; 1234]);
        s.dir("dir/matchdir");
        symlink(s.path().join("dir/matchdir"), s.path().join("dir/match-link")).unwrap();
        let mut got = Vec::new();
        walk(s.path(), "match", false, LIMIT, &mut |st| {
            if let Step::Found(f) = st {
                got.push((f.path.file_name().unwrap().to_string_lossy().into_owned(), f.kind, f.size, f.mtime > 0));
            }
            true
        })
        .unwrap();
        got.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(got[0].1, Kind::LinkDir);
        assert_eq!((got[1].1, got[1].2, got[1].3), (Kind::File, Some(1234), true));
        assert_eq!(got[2].1, Kind::Dir);
    }

    #[test]
    fn hidden_names_and_whatever_is_in_hidden_folders_only_show_when_asked() {
        let s = Scratch::new();
        s.file(".cache/thing.txt", b"");
        s.file(".thing", b"");
        s.file("thing.md", b"");
        assert_eq!(search(s.path(), "thing", false, LIMIT).0, ["thing.md"]);
        let (mut all, _) = search(s.path(), "thing", true, LIMIT);
        all.sort();
        assert_eq!(all, [".cache/thing.txt", ".thing", "thing.md"]);
    }

    #[test]
    fn symlinked_folders_arent_followed_but_still_match() {
        let s = Scratch::new();
        s.file("elsewhere/found-there.txt", b"");
        s.dir("top");
        symlink(s.path().join("elsewhere"), s.path().join("top/found-link")).unwrap();
        // The link's own name matches; what it leads to isn't read.
        assert_eq!(search(&s.path().join("top"), "found", false, LIMIT).0, ["found-link"]);
    }

    #[test]
    fn the_top_folder_can_be_a_symlink() {
        let s = Scratch::new();
        s.file("real/in/here.txt", b"");
        symlink(s.path().join("real"), s.path().join("link")).unwrap();
        assert_eq!(search(&s.path().join("link"), "here", false, LIMIT).0, ["in/here.txt"]);
    }

    #[test]
    fn another_mount_inside_isnt_read() {
        // In the test sandbox /run/gannet-tree/mnt is a tmpfs of its own inside /run's.
        let base = Path::new("/run/gannet-tree").join(format!("search-{}", std::process::id()));
        let other = Path::new("/run/gannet-tree/mnt").join(format!("search-{}", std::process::id()));
        std::fs::create_dir_all(base.join("sub")).unwrap();
        std::fs::write(base.join("sub/needle-near"), b"").unwrap();
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join("needle-far"), b"").unwrap();
        let (found, _) = search(Path::new("/run/gannet-tree"), "needle", false, LIMIT);
        std::fs::remove_dir_all(&base).unwrap();
        std::fs::remove_dir_all(&other).unwrap();
        assert!(found.iter().any(|f| f.ends_with("sub/needle-near")), "{found:?}");
        assert!(!found.iter().any(|f| f.contains("needle-far")), "it went into the other mount: {found:?}");
        // From inside that mount, it's searched as any folder is.
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join("needle-far"), b"").unwrap();
        let (inside, _) = search(Path::new("/run/gannet-tree/mnt"), "needle", false, LIMIT);
        std::fs::remove_dir_all(&other).unwrap();
        assert!(inside.iter().any(|f| f.ends_with("needle-far")), "{inside:?}");
    }

    #[test]
    fn it_stops_at_the_limit_and_says_so() {
        let s = Scratch::new();
        for i in 0..30 {
            s.file(&format!("d{}/hit-{i}", i % 3), b"");
        }
        let (found, walked) = search(s.path(), "hit", false, 10);
        assert_eq!(found.len(), 10);
        assert!(walked.truncated);
        let (all, walked) = search(s.path(), "hit", false, 30);
        assert_eq!((all.len(), walked.truncated), (30, true), "the limit reached exactly is still the limit");
        let (all, walked) = search(s.path(), "hit", false, 31);
        assert_eq!((all.len(), walked.truncated), (30, false));
    }

    #[test]
    fn it_stops_partway_when_told() {
        let s = Scratch::new();
        for i in 0..2000 {
            s.file(&format!("d{}/f{i}", i % 40), b"");
        }
        let mut ticks = 0;
        let r = walk(s.path(), "nothing-matches", false, LIMIT, &mut |st| {
            if matches!(st, Step::Tick) {
                ticks += 1;
            }
            ticks < 3
        })
        .unwrap();
        assert!(r.is_none());
        assert_eq!(ticks, 3, "it stopped at once");
    }

    #[test]
    fn it_stops_at_a_match_when_told() {
        let s = Scratch::new();
        for i in 0..5 {
            s.file(&format!("hit-{i}"), b"");
        }
        let mut seen = 0;
        let r = walk(s.path(), "hit", false, LIMIT, &mut |st| {
            if matches!(st, Step::Found(_)) {
                seen += 1;
                return false;
            }
            true
        })
        .unwrap();
        assert!(r.is_none());
        assert_eq!(seen, 1);
    }

    #[test]
    fn a_folder_that_cant_be_read_is_counted_and_the_rest_is_still_searched() {
        let s = Scratch::new();
        s.file("open/hit-1", b"");
        let locked = s.dir("locked");
        s.file("locked/hit-2", b"");
        std::fs::set_permissions(&locked, std::os::unix::fs::PermissionsExt::from_mode(0o000)).unwrap();
        let (found, walked) = search(s.path(), "hit", false, LIMIT);
        std::fs::set_permissions(&locked, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        assert!(found.contains(&"open/hit-1".to_string()));
        // Root can read it anyway.
        if found.len() == 1 {
            assert_eq!(walked.unreadable, 1);
        }
        assert!(walk(&s.path().join("missing"), "x", false, LIMIT, &mut |_| true).is_err());
    }
}
