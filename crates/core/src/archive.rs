//! Archives: which files Gannet unpacks itself rather than handing to another app, what the
//! folder they unpack into is called, and running `bsdtar` (from libarchive, which pacman
//! itself needs) to unpack one.
//!
//! `bsdtar` refuses, by default, any entry whose path has a `..` in it, and any entry that
//! would be written through a symlink, so nothing lands outside the folder it's given. It takes
//! a leading `/` off an absolute path, which keeps that inside the folder too. It does make
//! symlinks that point outside, so once it's done every symlink in the folder that will be put in
//! place is checked, and one that points anywhere but inside it is refused too. Any refusal makes the whole archive fail, and
//! the caller removes what was unpacked.

use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Seek};
use std::path::{Component, Path, PathBuf};
use std::os::unix::fs::{FileExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

/// The endings of the archives Gannet unpacks, longest first so `.tar.gz` wins over `.gz`.
/// Plain `.gz` or `.xz` files hold one compressed file rather than a folder of them, and
/// zip-based documents (`.docx`, `.jar`, `.apk`) belong to their own apps, so neither is here.
/// Disk images and packages aren't either: those are usually written to a drive or installed.
const ENDINGS: &[&str] = &[
    ".tar.gz", ".tar.bz2", ".tar.xz", ".tar.zst", ".tar.lz", ".tar.lzma", ".tar.z", ".tgz", ".tbz2", ".tbz", ".txz", ".tzst", ".tlz", ".tar", ".zip", ".7z", ".rar", ".cpio",
];

fn ending(name: &[u8]) -> Option<usize> {
    ENDINGS.iter().find(|e| name.len() > e.len() && name[name.len() - e.len()..].eq_ignore_ascii_case(e.as_bytes())).map(|e| e.len())
}

/// Whether a file called `name` is an archive Gannet unpacks when it's opened.
pub fn is_archive(name: &[u8]) -> bool {
    ending(name).is_some()
}

/// What the folder an archive unpacks into is called: its name without the archive ending.
pub fn folder_name(name: &[u8]) -> Vec<u8> {
    let stem = &name[..name.len() - ending(name).unwrap_or(0)];
    // Only dots would make a hidden folder, or `.` or `..`.
    if stem.iter().all(|&c| c == b'.') { b"Archive".to_vec() } else { stem.to_vec() }
}

/// Why an archive didn't unpack.
#[derive(Debug)]
pub enum Failed {
    Cancelled,
    /// `bsdtar` isn't installed.
    NoTool,
    /// It needs a password, which there's no way to give yet.
    Password,
    /// It has entries that would land outside its folder.
    Unsafe,
    NoSpace,
    /// Damaged, or not a kind `bsdtar` reads.
    Unreadable,
    Io(io::Error),
}

impl Failed {
    /// What to tell the user.
    pub fn message(&self) -> String {
        match self {
            Failed::Cancelled => "Stopped".into(),
            Failed::NoTool => "Extracting needs bsdtar, from the libarchive package, which isn't installed".into(),
            Failed::Password => "It's protected with a password, which Gannet can't open yet, so nothing was extracted".into(),
            Failed::Unsafe => "It holds items that would land outside its folder, so nothing was extracted".into(),
            Failed::NoSpace => "There isn't enough space on the disk, so nothing was extracted".into(),
            Failed::Unreadable => "It's damaged, or not a kind of archive Gannet can read, so nothing was extracted".into(),
            Failed::Io(e) => crate::listing::describe(e),
        }
    }

    /// Sorts out what `bsdtar` said on a failure, in the C.UTF-8 locale.
    fn from_messages(lines: &[String]) -> Failed {
        let said = |s: &str| lines.iter().any(|l| l.contains(s));
        if said("passphrase") || said("Passphrase") || said("Encrypted") || said("encrypted") {
            Failed::Password
        } else if said("Path contains '..'") || said("through symlink") || said("Hard-link target") {
            Failed::Unsafe
        } else if said("No space left") || said("Disk quota") {
            Failed::NoSpace
        } else {
            Failed::Unreadable
        }
    }
}

/// Unpacks `archive` into the folder `into`, with the program `tool` (`bsdtar`, but for tests).
/// `on_entry` hears each entry's name as it's written. `keep_going` hears how far into the
/// archive it has read, about every 50ms, and stops it by answering false.
pub fn unpack(tool: &OsStr, archive: &File, into: &Path, mut on_entry: impl FnMut(&str), mut keep_going: impl FnMut(u64) -> bool) -> Result<(), Failed> {
    // The archive is its input, so the two share one read position, and that is the progress.
    // It's a regular file, so bsdtar can still seek in it, as zip and 7z need.
    let input = archive.try_clone().map_err(Failed::Io)?;
    let mut reading = archive.try_clone().map_err(Failed::Io)?;
    reading.rewind().map_err(Failed::Io)?;
    let mut command = Command::new(tool);
    command
        // A password is never asked for: there's nowhere to type it, and this one fails at once.
        .args(["-x", "-v", "--no-same-owner", "--no-same-permissions", "--passphrase", "gannet", "-f", "-", "-C"])
        .arg(into)
        // UTF-8, or names that aren't ASCII fail to convert; C.UTF-8 keeps its messages in English.
        .env("LC_ALL", "C.UTF-8")
        .stdin(input)
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    // If Gannet is killed, bsdtar goes with it rather than unpacking on into a hidden folder. That
    // follows the thread that started it, which is this one, waiting below until it ends.
    // SAFETY: prctl is async-signal-safe and touches no memory.
    unsafe {
        command.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
            Ok(())
        });
    }
    let mut child = command
        .spawn()
        .map_err(|e| if e.kind() == io::ErrorKind::NotFound { Failed::NoTool } else { Failed::Io(e) })?;
    // Read on its own thread, so a long list of names can never fill the pipe and stall it.
    let (tx, rx) = mpsc::channel();
    let stderr = child.stderr.take().expect("stderr is piped");
    let reader = std::thread::Builder::new()
        .name("gannet-unpack".into())
        .spawn(move || {
            for line in BufReader::new(stderr).split(b'\n').map_while(Result::ok) {
                if tx.send(String::from_utf8_lossy(&line).into_owned()).is_err() {
                    break;
                }
            }
        })
        .map_err(Failed::Io)?;
    let mut problems = Vec::new();
    let mut sort = |line: String, problems: &mut Vec<String>| match line.strip_prefix("x ") {
        // An entry that failed is listed with its reason after a colon.
        Some(name) if !name.contains(": ") => on_entry(name),
        _ => problems.push(line),
    };
    let status = loop {
        while let Ok(line) = rx.try_recv() {
            sort(line, &mut problems);
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return Err(Failed::Io(e));
            }
        }
        let at = reading.stream_position().unwrap_or(0);
        if !keep_going(at) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader.join();
            return Err(Failed::Cancelled);
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let _ = reader.join();
    while let Ok(line) = rx.try_recv() {
        sort(line, &mut problems);
    }
    if status.success() {
        return Ok(());
    }
    Err(match Failed::from_messages(&problems) {
        // A zip's old ZipCrypto lock checks a password against one byte, so about one archive
        // in 256 takes the made-up passphrase above, then fails to decompress, which reads as
        // damage. The zip itself says whether it's locked (ALI-2187). A locked zip that's also
        // damaged reads as locked too: the two can't be told apart, and it can't be opened either way.
        Failed::Unreadable if zip_is_locked(archive) => Failed::Password,
        failed => failed,
    })
}

/// Whether `archive` is a zip that says any of its entries is encrypted: bit 0 of an entry's
/// flags, set for ZipCrypto and for WinZip's AES alike. Reads the central directory at the end
/// of the file; a zip cut short has none, so then the first entry's own header decides.
fn zip_is_locked(archive: &File) -> bool {
    const LIMIT: u64 = 64 << 20;
    let Ok(len) = archive.metadata().map(|m| m.len()) else { return false };
    let read = |at: u64, n: u64| -> Option<Vec<u8>> {
        let mut buf = vec![0; usize::try_from(n).ok()?];
        archive.read_exact_at(&mut buf, at).ok().map(|()| buf)
    };
    let u16_at = |b: &[u8], i: usize| u16::from_le_bytes([b[i], b[i + 1]]);
    let u32_at = |b: &[u8], i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);

    // The end record is the last 22 bytes, or up to 64 KiB earlier with a comment after it.
    let directory_says_locked = || -> Option<bool> {
        let tail_len = len.min(22 + 0xffff);
        let tail = read(len - tail_len, tail_len)?;
        let end = tail.windows(4).rposition(|w| w == b"PK\x05\x06").filter(|&i| i + 22 <= tail.len())?;
        let (size, offset) = (u64::from(u32_at(&tail, end + 12)), u64::from(u32_at(&tail, end + 16)));
        if size > LIMIT || offset.checked_add(size).is_none_or(|e| e > len) {
            return None;
        }
        let dir = read(offset, size)?;
        let mut at = 0;
        while at + 46 <= dir.len() && &dir[at..at + 4] == b"PK\x01\x02" {
            if u16_at(&dir, at + 8) & 1 == 1 {
                return Some(true);
            }
            at += 46 + usize::from(u16_at(&dir, at + 28)) + usize::from(u16_at(&dir, at + 30)) + usize::from(u16_at(&dir, at + 32));
        }
        Some(false)
    };
    if directory_says_locked() == Some(true) {
        return true;
    }
    read(0, 30.min(len)).is_some_and(|head| head.len() == 30 && &head[..4] == b"PK\x03\x04" && u16_at(&head, 6) & 1 == 1)
}

/// The folders `check_links` had to open up, each with the mode it had, from the top down.
/// `restore_modes` puts them back, once the folder is where it's staying.
pub type Opened = Vec<(PathBuf, u32)>;

/// Checks that every symlink under `root`, the folder that will be put in place, points
/// somewhere inside it. Stops if `cancel` is set.
///
/// It works in two passes. The first gives every folder its owner's read, write and search
/// bits, listing each one it changed so its mode can be put back: a link's lookup must never
/// meet a folder it can't see into, and the folder put in place has to be writable to be moved
/// to another. The second resolves every link. Neither follows a link while walking.
pub fn check_links(root: &Path, cancel: &AtomicBool) -> Result<Opened, Failed> {
    let root = root.canonicalize().map_err(Failed::Io)?;
    let mut opened = Vec::new();
    let checked = open_up(&root, &root, cancel, &mut opened).and_then(|()| links_inside(&root, &root, cancel));
    match checked {
        Ok(true) => Ok(opened),
        Ok(false) => {
            restore_modes(&root, &opened);
            Err(Failed::Unsafe)
        }
        Err(e) => {
            restore_modes(&root, &opened);
            Err(e)
        }
    }
}

/// Puts back the modes `check_links` changed, now under `root`, deepest first so each folder
/// can still be reached while it's done.
/// The folder at `root` keeps write for its owner: without it, it couldn't be moved again, so
/// undo couldn't put it in the Trash.
pub fn restore_modes(root: &Path, opened: &Opened) {
    for (rel, mode) in opened.iter().rev() {
        let mode = if rel.as_os_str().is_empty() { mode | 0o200 } else { *mode };
        let _ = std::fs::set_permissions(root.join(rel), std::fs::Permissions::from_mode(mode));
    }
}

fn open_up(root: &Path, dir: &Path, cancel: &AtomicBool, opened: &mut Opened) -> Result<(), Failed> {
    let mode = std::fs::symlink_metadata(dir).map_err(Failed::Io)?.permissions().mode() & 0o7777;
    if mode & 0o700 != 0o700 {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode | 0o700)).map_err(Failed::Io)?;
        opened.push((dir.strip_prefix(root).unwrap_or(Path::new("")).to_path_buf(), mode));
    }
    for entry in std::fs::read_dir(dir).map_err(Failed::Io)? {
        if cancel.load(Ordering::Relaxed) {
            return Err(Failed::Cancelled);
        }
        let entry = entry.map_err(Failed::Io)?;
        // The entry's own type, so a link to a folder is never walked into or changed.
        if entry.file_type().map_err(Failed::Io)?.is_dir() {
            open_up(root, &entry.path(), cancel, opened)?;
        }
    }
    Ok(())
}

fn links_inside(root: &Path, dir: &Path, cancel: &AtomicBool) -> Result<bool, Failed> {
    for entry in std::fs::read_dir(dir).map_err(Failed::Io)? {
        if cancel.load(Ordering::Relaxed) {
            return Err(Failed::Cancelled);
        }
        let entry = entry.map_err(Failed::Io)?;
        let kind = entry.file_type().map_err(Failed::Io)?;
        let inside = if kind.is_symlink() {
            let target = std::fs::read_link(entry.path()).map_err(Failed::Io)?;
            // One budget for the whole lookup, as the kernel keeps, so links that each lead to
            // two more can't make it take forever.
            let mut hops = 40;
            resolve(root, dir.to_path_buf(), &target, &mut hops).is_some()
        } else if kind.is_dir() {
            links_inside(root, &entry.path(), cancel)?
        } else {
            true
        };
        if !inside {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Where `target`, read as a link in the folder `at`, leads, or None if it leaves `root` at any
/// step. Links on the way are followed, as the kernel would, until `hops` runs out, so a link to
/// `.` can't make a later `..` look shallower than it is. A part that doesn't exist is taken as
/// written. Any other failure to look at a part refuses it, since what it hides could be a link.
/// An absolute target is refused outright, even one that names a place inside.
fn resolve(root: &Path, mut at: PathBuf, target: &Path, hops: &mut u32) -> Option<PathBuf> {
    for part in target.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                if at == root {
                    return None;
                }
                at.pop();
            }
            Component::Normal(name) => {
                let next = at.join(name);
                match std::fs::symlink_metadata(&next) {
                    Ok(m) if m.file_type().is_symlink() => {
                        *hops = hops.checked_sub(1)?;
                        let further = std::fs::read_link(&next).ok()?;
                        at = resolve(root, at, &further, hops)?;
                    }
                    Ok(_) => at = next,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => at = next,
                    Err(_) => return None,
                }
            }
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(at)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archives_are_known_by_their_endings_in_any_case() {
        for n in ["a.zip", "a.ZIP", "a.tar.gz", "b.tgz", "c.tar.zst", "d.7z", "e.rar", "f.tar", "g.Tar.Xz"] {
            assert!(is_archive(n.as_bytes()), "{n}");
        }
        for n in ["a.gz", "report.docx", "app.jar", "disk.iso", "pkg.deb", "zip", ".zip", "a.zip.txt", "notes"] {
            assert!(!is_archive(n.as_bytes()), "{n}");
        }
    }

    #[test]
    fn the_folder_is_named_after_the_archive() {
        assert_eq!(folder_name(b"photos.zip"), b"photos");
        assert_eq!(folder_name(b"src-1.2.tar.gz"), b"src-1.2");
        assert_eq!(folder_name(b"Backup.TGZ"), b"Backup");
        assert_eq!(folder_name(b"...zip"), b"Archive");
    }

    #[test]
    fn one_budget_of_hops_covers_a_whole_lookup() {
        let s = crate::testutil::Scratch::new();
        let root = s.path().canonicalize().unwrap();
        // l0 -> l1/l1, l1 -> l2/l2 … l30 -> . takes twice the hops at each level: over 40 in all
        // from l25 down, as it would for the kernel, so l0 is refused, and quickly.
        for i in 0..30 {
            std::os::unix::fs::symlink(format!("l{0}/l{0}", i + 1), root.join(format!("l{i}"))).unwrap();
        }
        std::os::unix::fs::symlink(".", root.join("l30")).unwrap();
        let started = std::time::Instant::now();
        let at = |name: &str| resolve(&root, root.clone(), Path::new(name), &mut 40);
        assert_eq!(at("l26"), Some(root.clone()));
        assert_eq!(at("l25"), None);
        assert_eq!(at("l0"), None);
        assert!(started.elapsed() < Duration::from_secs(2), "took {:?}", started.elapsed());
    }

    #[test]
    fn a_link_through_a_folder_not_yet_opened_is_still_followed() {
        use std::os::unix::fs::symlink;
        // proj/d/x -> .. stays inside, but a link -> d/x/.. leads out of proj. With d shut, a
        // lookup that can't see into it takes x as a plain name. The link is made before and
        // after d, and named to sort either side of it, so whatever order the folder lists them
        // in, some case checks the link before d is reached.
        for (name, link_first) in [("a", true), ("a", false), ("zz", true), ("zz", false)] {
            let s = crate::testutil::Scratch::new();
            let root = s.path().join("proj");
            std::fs::create_dir(&root).unwrap();
            let link = || symlink("d/x/..", root.join(name)).unwrap();
            if link_first {
                link();
            }
            std::fs::create_dir(root.join("d")).unwrap();
            symlink("..", root.join("d/x")).unwrap();
            if !link_first {
                link();
            }
            std::fs::set_permissions(root.join("d"), std::fs::Permissions::from_mode(0o000)).unwrap();
            let found = check_links(&root, &AtomicBool::new(false));
            let mode = std::fs::symlink_metadata(root.join("d")).unwrap().permissions().mode() & 0o7777;
            std::fs::set_permissions(root.join("d"), std::fs::Permissions::from_mode(0o700)).unwrap();
            assert!(matches!(found, Err(Failed::Unsafe)), "{name}, link first {link_first}: {found:?}");
            assert_eq!(mode, 0o000, "d gets its mode back");
        }
    }

    /// A ZipCrypto zip whose one check byte happens to take the passphrase `unpack` tries, so
    /// bsdtar gets as far as decompressing and says only that that failed (ALI-2187). About one
    /// newly made locked zip in 256 is like this, which made a test flaky. Its password is
    /// "secret".
    const LOCKED_BUT_CHECK_BYTE_PASSES: &[u8] = include_bytes!("../tests/data/locked-check-byte-passes.zip");

    fn unpack_bytes(bytes: &[u8]) -> Result<(), Failed> {
        let s = crate::testutil::Scratch::new();
        let archive = File::open(s.file("a.zip", bytes)).unwrap();
        unpack(OsStr::new("bsdtar"), &archive, &s.dir("out"), |_| {}, |_| true)
    }

    #[test]
    fn a_locked_zip_says_password_even_when_its_check_byte_takes_the_wrong_one() {
        let got = unpack_bytes(LOCKED_BUT_CHECK_BYTE_PASSES);
        assert!(matches!(got, Err(Failed::Password)), "{got:?}");
    }

    #[test]
    fn a_zip_tells_whether_it_is_locked() {
        let s = crate::testutil::Scratch::new();
        let locked = |bytes: &[u8]| zip_is_locked(&File::open(s.file("z.zip", bytes)).unwrap());
        assert!(locked(LOCKED_BUT_CHECK_BYTE_PASSES));
        // Cut short, so there's no central directory: the first entry's header says so.
        assert!(locked(&LOCKED_BUT_CHECK_BYTE_PASSES[..LOCKED_BUT_CHECK_BYTE_PASSES.len() / 2]));
        s.file("src/a.txt", b"plain");
        let plain = s.path().join("plain.zip");
        let made = Command::new("bsdtar").args(["--format", "zip", "-cf"]).arg(&plain).arg("-C").arg(s.path().join("src")).arg("a.txt").status().unwrap();
        assert!(made.success());
        assert!(!locked(&std::fs::read(&plain).unwrap()));
        assert!(!locked(b"this was never a zip file"));
        assert!(!locked(b""));
    }

    #[test]
    fn a_damaged_zip_that_isnt_locked_still_says_damaged() {
        let got = unpack_bytes(b"PK\x03\x04 not really a zip");
        assert!(matches!(got, Err(Failed::Unreadable)), "{got:?}");
    }

    #[test]
    fn what_bsdtar_says_is_sorted_into_reasons() {
        let one = |s: &str| Failed::from_messages(&[s.to_string()]);
        assert!(matches!(one("x a.txt: Incorrect passphrase: Unknown error -1"), Failed::Password));
        assert!(matches!(one("x ../evil: Path contains '..': Unknown error -1"), Failed::Unsafe));
        assert!(matches!(one("x ln/x: Cannot extract through symlink ln/x"), Failed::Unsafe));
        assert!(matches!(one("x big: Write failed: No space left on device"), Failed::NoSpace));
        assert!(matches!(one("bsdtar: Missing type keyword in mtree specification"), Failed::Unreadable));
    }
}
