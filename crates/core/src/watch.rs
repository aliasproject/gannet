//! A thin inotify wrapper. The fd goes in the app's poll set; nothing here blocks.

use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rustix::fs::inotify::{self, CreateFlags, ReadFlags, WatchFlags};
use rustix::io::Errno;

/// Shareable between threads. Adding a watch looks up a path, which can hang on a dead
/// mount, so that happens with no lock held; `drain` only reads the fd.
pub struct Watch {
    fd: OwnedFd,
    current: Mutex<Vec<(PathBuf, i32)>>,
}

impl Watch {
    pub fn new() -> rustix::io::Result<Self> {
        Ok(Self {
            fd: inotify::init(CreateFlags::CLOEXEC | CreateFlags::NONBLOCK)?,
            current: Mutex::new(Vec::new()),
        })
    }

    /// Watches these folders for entries changing. Folders already watched keep their
    /// watch, so no change there can slip by, and a path whose folder got replaced moves
    /// to the new one. One that can't be watched is skipped: the app still works, it
    /// just needs a manual reload.
    pub fn set_dirs<P: AsRef<Path>>(&self, dirs: &[P]) {
        self.set_dirs_unless(dirs, || false);
    }

    /// As `set_dirs`, but does nothing further if `stale` says this request has been
    /// overtaken while the lookups ran.
    pub fn set_dirs_unless<P: AsRef<Path>>(&self, dirs: &[P], stale: impl Fn() -> bool) {
        let flags = WatchFlags::CREATE
            | WatchFlags::DELETE
            | WatchFlags::MOVED_FROM
            | WatchFlags::MOVED_TO
            | WatchFlags::CLOSE_WRITE
            | WatchFlags::ATTRIB
            | WatchFlags::DELETE_SELF
            | WatchFlags::MOVE_SELF
            | WatchFlags::ONLYDIR;
        // Adding is idempotent: the same inode gets the same watch back, and a replaced
        // folder gets a new one. Adds come before removes so nothing goes unwatched.
        let mut next: Vec<(PathBuf, i32)> = Vec::with_capacity(dirs.len());
        for d in dirs {
            let d = d.as_ref();
            if let Ok(wd) = inotify::add_watch(&self.fd, d, flags) {
                next.push((d.to_path_buf(), wd));
            }
        }
        let Ok(mut current) = self.current.lock() else {
            return;
        };
        if stale() {
            // A newer request owns the list now. Anything only this one added has to go.
            for (_, wd) in &next {
                if !current.iter().any(|(_, w)| w == wd) {
                    let _ = inotify::remove_watch(&self.fd, *wd);
                }
            }
            return;
        }
        let old = std::mem::replace(&mut *current, next);
        for (_, wd) in old {
            if !current.iter().any(|(_, w)| *w == wd) {
                let _ = inotify::remove_watch(&self.fd, wd);
            }
        }
    }

    /// Reads everything pending; true if anything changed. Removing a watch queues an
    /// "ignored" event, which isn't a change and doesn't count.
    pub fn drain(&self) -> bool {
        let mut any = false;
        let mut buf = [std::mem::MaybeUninit::<u8>::uninit(); 4096];
        loop {
            let mut reader = inotify::Reader::new(&self.fd, &mut buf);
            match reader.next() {
                Ok(ev) => {
                    any |= !ev.events().contains(ReadFlags::IGNORED);
                    while let Ok(ev) = reader.next() {
                        any |= !ev.events().contains(ReadFlags::IGNORED);
                    }
                }
                Err(Errno::INTR) => continue,
                Err(_) => return any,
            }
        }
    }
}

impl AsFd for Watch {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::Scratch;

    #[test]
    fn notices_create_and_delete() {
        let s = Scratch::new();
        let w = Watch::new().unwrap();
        w.set_dirs(&[s.path()]);
        assert!(!w.drain());
        let f = s.file("a", b"x");
        assert!(w.drain());
        assert!(!w.drain());
        std::fs::remove_file(f).unwrap();
        assert!(w.drain());
    }

    #[test]
    fn a_folder_in_both_sets_keeps_its_watch() {
        let (a, b) = (Scratch::new(), Scratch::new());
        let w = Watch::new().unwrap();
        w.set_dirs(&[a.path()]);
        let before = w.current.lock().unwrap()[0].1;
        w.set_dirs(&[a.path(), b.path()]);
        let current = w.current.lock().unwrap();
        assert_eq!(
            current.iter().find(|(p, _)| p == a.path()).unwrap().1,
            before
        );
        assert_eq!(current.len(), 2);
    }

    #[test]
    fn a_replaced_folder_at_the_same_path_gets_a_new_watch() {
        let s = Scratch::new();
        let d = s.dir("x");
        let w = Watch::new().unwrap();
        w.set_dirs(&[&d]);
        std::fs::remove_dir(&d).unwrap();
        std::fs::create_dir(&d).unwrap();
        w.drain();
        w.set_dirs(&[&d]);
        w.drain();
        std::fs::write(d.join("f"), b"").unwrap();
        assert!(w.drain());
    }

    #[test]
    fn a_stale_request_leaves_the_current_watch_alone() {
        let (a, b) = (Scratch::new(), Scratch::new());
        let w = Watch::new().unwrap();
        w.set_dirs(&[a.path()]);
        w.set_dirs_unless(&[b.path()], || true);
        w.drain();
        b.file("y", b"");
        assert!(!w.drain(), "the stale folder's watch must be gone");
        a.file("x", b"");
        assert!(w.drain(), "the current one must still work");
    }

    #[test]
    fn replacing_dirs_stops_watching_the_old_one() {
        let (a, b) = (Scratch::new(), Scratch::new());
        let w = Watch::new().unwrap();
        w.set_dirs(&[a.path()]);
        w.set_dirs(&[b.path()]);
        assert!(!w.drain(), "changing watches is not a change");
        a.file("x", b"");
        assert!(!w.drain());
        b.file("y", b"");
        assert!(w.drain());
    }
}
