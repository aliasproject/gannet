//! Archives: which files Gannet unpacks itself rather than handing to another app, what the
//! folder they unpack into is called, and running `bsdtar` (from libarchive, which pacman
//! itself needs) to unpack one.
//!
//! `bsdtar` refuses, by default, any entry whose path has a `..` in it, and any entry that
//! would be written through a symlink, so nothing lands outside the folder it's given. It takes
//! a leading `/` off an absolute path, which keeps that inside the folder too. Any refusal makes
//! the whole archive fail, and the caller removes what was unpacked.

use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Seek};
use std::path::Path;
use std::process::{Command, Stdio};
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

    /// Sorts out what `bsdtar` said on a failure, in the C locale.
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
    let mut child = Command::new(tool)
        // A password is never asked for: there's nowhere to type it, and this one fails at once.
        .args(["-x", "-v", "--no-same-owner", "--no-same-permissions", "--passphrase", "gannet", "-f", "-", "-C"])
        .arg(into)
        .env("LC_ALL", "C")
        .stdin(input)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
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
        if let Some(status) = child.try_wait().map_err(Failed::Io)? {
            break status;
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
    if status.success() { Ok(()) } else { Err(Failed::from_messages(&problems)) }
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
    fn what_bsdtar_says_is_sorted_into_reasons() {
        let one = |s: &str| Failed::from_messages(&[s.to_string()]);
        assert!(matches!(one("x a.txt: Incorrect passphrase: Unknown error -1"), Failed::Password));
        assert!(matches!(one("x ../evil: Path contains '..': Unknown error -1"), Failed::Unsafe));
        assert!(matches!(one("x ln/x: Cannot extract through symlink ln/x"), Failed::Unsafe));
        assert!(matches!(one("x big: Write failed: No space left on device"), Failed::NoSpace));
        assert!(matches!(one("bsdtar: Missing type keyword in mtree specification"), Failed::Unreadable));
    }
}
