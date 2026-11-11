//! What a folder holds: how many items are in it, and how much everything under it adds up to.
//! Counting a big tree takes a while, so it reports as it goes and stops when asked.
//!
//! Like `du`, it doesn't follow symlinks or cross onto other disks, and a file with several hard
//! links counts once. Sizes are what the files say they hold, as the list shows them.

use std::collections::HashSet;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How far the count has got.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Tally {
    /// Items directly in the folder.
    pub items: u64,
    /// Bytes in everything under it, however deep.
    pub bytes: u64,
    /// Something inside couldn't be read, so `bytes` is at least this much.
    pub incomplete: bool,
    pub done: bool,
}

/// How often a count that's still going says how far it's got.
const REPORT_EVERY: Duration = Duration::from_millis(150);
/// Entries between looks at whether to stop, which costs a system call.
const CHECK_EVERY: u32 = 256;

/// Counts `dir`, telling `report` how far it's got now and then. Returns the whole tally, or `None`
/// if `stop` said to stop.
pub fn measure(dir: &Path, stop: &dyn Fn() -> bool, report: &mut dyn FnMut(Tally)) -> Option<Tally> {
    let mut tally = Tally::default();
    let Ok(top) = std::fs::symlink_metadata(dir) else {
        tally.incomplete = true;
        tally.done = true;
        return Some(tally);
    };
    let dev = top.dev();
    let mut seen: HashSet<(u64, u64)> = HashSet::new();
    // Folders still to read, deepest last, so memory follows the tree's width rather than its size.
    let mut pending: Vec<(PathBuf, bool)> = vec![(dir.to_path_buf(), true)];
    let mut last_report = Instant::now();
    let mut since_check = 0u32;
    while let Some((folder, is_top)) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&folder) else {
            tally.incomplete = true;
            continue;
        };
        for entry in entries {
            since_check += 1;
            if since_check >= CHECK_EVERY {
                since_check = 0;
                if stop() {
                    return None;
                }
                if last_report.elapsed() >= REPORT_EVERY {
                    last_report = Instant::now();
                    report(tally);
                }
            }
            let Ok(entry) = entry else {
                tally.incomplete = true;
                continue;
            };
            if is_top {
                tally.items += 1;
            }
            let Ok(meta) = entry.metadata() else {
                tally.incomplete = true;
                continue;
            };
            if meta.is_dir() {
                // Another disk mounted inside is that disk's size, not this folder's.
                if meta.dev() == dev {
                    pending.push((entry.path(), false));
                }
                continue;
            }
            if meta.nlink() > 1 && !seen.insert((meta.dev(), meta.ino())) {
                continue;
            }
            if meta.is_file() {
                tally.bytes += meta.len();
            }
        }
    }
    if stop() {
        return None;
    }
    tally.done = true;
    Some(tally)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::Scratch;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn count(dir: &Path) -> Tally {
        measure(dir, &|| false, &mut |_| {}).unwrap()
    }

    #[test]
    fn items_are_counted_at_the_top_and_sizes_all_the_way_down() {
        let s = Scratch::new();
        s.file("top/a", &[0; 10]);
        s.file("top/.hidden", &[0; 5]);
        s.file("top/sub/b", &[0; 100]);
        s.file("top/sub/deeper/c", &[0; 1000]);
        s.dir("top/empty");
        let t = count(&s.path().join("top"));
        assert_eq!(t, Tally { items: 4, bytes: 1115, incomplete: false, done: true });
    }

    #[test]
    fn symlinks_arent_followed_and_hard_links_count_once() {
        let s = Scratch::new();
        let big = s.file("elsewhere/big", &[0; 5000]);
        s.file("top/a", &[0; 10]);
        std::os::unix::fs::symlink(s.path().join("elsewhere"), s.path().join("top/link")).unwrap();
        std::fs::hard_link(s.path().join("top/a"), s.path().join("top/a2")).unwrap();
        let t = count(&s.path().join("top"));
        assert_eq!((t.items, t.bytes), (3, 10));
        assert!(big.exists());
    }

    #[test]
    fn a_folder_that_cant_be_read_is_said_to_be_missing_from_the_total() {
        let s = Scratch::new();
        s.file("top/a", &[0; 10]);
        let locked = s.dir("top/locked");
        s.file("top/locked/secret", &[0; 99]);
        std::fs::set_permissions(&locked, std::os::unix::fs::PermissionsExt::from_mode(0o000)).unwrap();
        let t = count(&s.path().join("top"));
        std::fs::set_permissions(&locked, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        // Root can read it anyway, and then it's all there.
        if t.bytes == 10 {
            assert!(t.incomplete);
        }
        assert_eq!(t.items, 2);
        let gone = count(&s.path().join("nothing-here"));
        assert!(gone.incomplete && gone.done);
    }

    #[test]
    fn a_big_tree_stops_partway_when_asked() {
        let s = Scratch::new();
        for i in 0..2000 {
            s.file(&format!("top/d{}/f{i}", i % 20), &[0; 3]);
        }
        let asked = AtomicBool::new(false);
        let stopped = measure(&s.path().join("top"), &|| asked.swap(true, Ordering::Relaxed), &mut |_| {});
        assert!(stopped.is_none(), "it looked partway, not only at the end");
        let whole = count(&s.path().join("top"));
        assert_eq!((whole.items, whole.bytes), (20, 6000));
    }
}
