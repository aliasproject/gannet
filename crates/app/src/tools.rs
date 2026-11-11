//! Outside programs that read files for the preview: poppler for PDF pages, and the system's
//! thumbnailers for kinds of file nothing else here can read. Each runs in the same bubblewrap
//! jail as the video player (see `media`), with fonts to draw text with, limits on its memory, CPU
//! time and output, and only the one file it was given. What it writes is a PNG, which goes to
//! the image decoder's own sandbox before anything here looks at it.
//!
//! The video player's seccomp filter isn't added: these are other people's programs, and some,
//! like glycin, start sandboxes of their own, which the filter would stop.

use std::ffi::OsString;
use std::fs::File;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::media;

/// Where a program has to be to run in the jail, which has only `/usr`.
pub const PROGRAMS: &str = "/usr/bin";
const SHELL: &str = "/usr/bin/sh";
/// Where the file appears inside the jail, for a program that wants a path rather than stdin.
const JAILED_INPUT: &str = "/in/file";
const JAILED_OUTPUT: &str = "/tmp/picture.png";
/// The descriptor the file is on when bubblewrap binds it.
const FD_INPUT: libc::c_int = 3;
const GIB: u64 = 1024 * 1024 * 1024;
const MAX_MEMORY: u64 = 4 * GIB;
const MAX_CPU_SECONDS: u64 = 30;
/// A page or a picture this big is past anything the decoder would read anyway.
const MAX_OUTPUT: u64 = 600 * 1024 * 1024;
/// Pages past this aren't listed: nobody scrolls that far in a preview.
pub const MAX_PAGES: usize = 5000;

/// Fonts, as poppler and office thumbnailers draw text, read-only, on top of the video jail.
const FONT_RULES: &[(&str, &[&str])] = &[("--ro-bind-try", &["/etc/fonts", "/etc/fonts"]), ("--ro-bind-try", &["/var/cache/fontconfig", "/var/cache/fontconfig"])];

/// A program to run jailed, with `input` on its stdin, or bound at a path inside if `bind` is
/// set (as `JAILED_INPUT` with `bind` on the end, which is the name's extension, for programs that
/// go by it). Its stdout and stderr are for the caller to set.
pub fn jailed(argv: &[OsString], input: File, bind: Option<&str>) -> std::io::Result<Command> {
    let mut command = Command::new("bwrap");
    let mut rule = |option: &str, values: &[&str]| {
        command.arg(option).args(values);
    };
    for (option, values) in media::JAIL_RULES.iter().chain(FONT_RULES) {
        rule(option, values);
    }
    let input_fd = match bind {
        Some(ext) => {
            // An open file rather than its path, so it can't be swapped for another on the way in.
            rule("--ro-bind-fd", &[&FD_INPUT.to_string(), &format!("{JAILED_INPUT}{ext}")]);
            Some(high(&input)?)
        }
        None => {
            command.stdin(Stdio::from(input));
            None
        }
    };
    command.arg("--").args(argv);
    // SAFETY: only async-signal-safe calls, on integers and descriptors this process owns. The
    // closure owns the copy of the input, so it's open until the command is spawned and dropped.
    unsafe {
        command.pre_exec(move || {
            let raw_input = input_fd.as_ref().map(|fd| fd.as_raw_fd());
            if let Some(fd) = raw_input
                && libc::dup2(fd, FD_INPUT) < 0
            {
                return Err(std::io::Error::last_os_error());
            }
            let first_unused = if raw_input.is_some() { FD_INPUT + 1 } else { 3 };
            if libc::close_range(first_unused as libc::c_uint, libc::c_uint::MAX, libc::CLOSE_RANGE_CLOEXEC as libc::c_int) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            for (resource, value) in [(libc::RLIMIT_AS, MAX_MEMORY), (libc::RLIMIT_CPU, MAX_CPU_SECONDS), (libc::RLIMIT_FSIZE, MAX_OUTPUT), (libc::RLIMIT_CORE, 0)] {
                let mut l = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
                if libc::getrlimit(resource, &mut l) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                let value = value.min(l.rlim_cur).min(l.rlim_max);
                l = libc::rlimit { rlim_cur: value, rlim_max: value };
                if libc::setrlimit(resource, &l) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    if bind.is_some() {
        command.stdin(Stdio::null());
    }
    Ok(command)
}

/// A copy of `file` well away from the numbers the jail's descriptors are about to be given.
fn high(file: &File) -> std::io::Result<OwnedFd> {
    // SAFETY: duplicating a descriptor we hold, and only taking ownership of a good result.
    let copy = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 64) };
    if copy < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: a new descriptor from the call above, owned by nothing else.
    Ok(unsafe { OwnedFd::from_raw_fd(copy) })
}

/// `pdfinfo` with its errors on stdout too, for telling a locked PDF from a broken one.
pub fn pdfinfo() -> Vec<OsString> {
    [SHELL, "-c", &format!("exec {PROGRAMS}/pdfinfo -f 1 -l {MAX_PAGES} - 2>&1")].map(OsString::from).to_vec()
}

/// `pdftoppm` drawing one page, `index` from 0, `width` pixels across, as a PNG on stdout.
pub fn pdftoppm(index: usize, width: i32) -> Vec<OsString> {
    let page = (index + 1).to_string();
    let width = width.max(1).to_string();
    [&format!("{PROGRAMS}/pdftoppm"), "-f", &page, "-l", &page, "-scale-to-x", &width, "-scale-to-y", "-1", "-png", "-"].map(OsString::from).to_vec()
}

/// A thumbnailer's command line, made to write into the jail's own `/tmp` and then copied to
/// stdout, as the jail has nowhere else to write. `ext` is the input's extension, as `jailed`
/// binds it.
pub fn thumbnailer(line: impl FnOnce(&Path, &Path) -> Option<Vec<OsString>>, ext: &str) -> Option<Vec<OsString>> {
    let input = PathBuf::from(format!("{JAILED_INPUT}{ext}"));
    let line = line(&input, Path::new(JAILED_OUTPUT))?;
    let mut argv: Vec<OsString> = [SHELL, "-c", &format!("\"$@\" && exec cat {JAILED_OUTPUT}"), "sh"].map(OsString::from).to_vec();
    argv.extend(line);
    Some(argv)
}

/// The extension to give the file inside the jail: its own, if it's a plain one.
pub fn extension(path: &Path) -> String {
    match path.extension().and_then(|e| e.to_str()) {
        Some(e) if !e.is_empty() && e.len() <= 10 && e.bytes().all(|b| b.is_ascii_alphanumeric()) => format!(".{e}"),
        _ => String::new(),
    }
}

#[derive(Debug, PartialEq)]
pub enum PdfInfo {
    /// Each page's width and height, turned as it's shown.
    Pages(Vec<(f32, f32)>),
    Locked,
    Unreadable,
}

/// What `pdfinfo` said, which is believed only as far as it makes sense.
pub fn parse_pdfinfo(out: &str, success: bool) -> PdfInfo {
    if out.contains("Incorrect password") {
        return PdfInfo::Locked;
    }
    if !success {
        return PdfInfo::Unreadable;
    }
    let mut pages: Vec<(f32, f32)> = Vec::new();
    for line in out.lines() {
        let Some(rest) = line.strip_prefix("Page") else { continue };
        let mut words = rest.split_whitespace();
        let Some(n) = words.next().and_then(|n| n.parse::<usize>().ok()) else { continue };
        if n == 0 || n > MAX_PAGES {
            continue;
        }
        match words.next() {
            Some("size:") => {
                let w = words.next().and_then(|w| w.parse::<f32>().ok());
                let h = words.nth(1).and_then(|h| h.parse::<f32>().ok());
                let (Some(w), Some(h)) = (w, h) else { continue };
                if !(w.is_finite() && h.is_finite() && w > 0.0 && h > 0.0 && w < 1e6 && h < 1e6) || n != pages.len() + 1 {
                    continue;
                }
                pages.push((w, h));
            }
            Some("rot:") => {
                let turned = words.next().and_then(|r| r.parse::<i32>().ok()).is_some_and(|r| r.rem_euclid(180) == 90);
                if turned && let Some(page) = pages.get_mut(n - 1) {
                    *page = (page.1, page.0);
                }
            }
            _ => {}
        }
    }
    if pages.is_empty() { PdfInfo::Unreadable } else { PdfInfo::Pages(pages) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pdfinfo_is_read_page_by_page() {
        let out = "Pages:           3\nPage    1 size:  612 x 792 pts (letter)\nPage    1 rot:   0\nPage    2 size:  842 x 595 pts (A4)\nPage    2 rot:   90\nPage    3 size:  612 x 792 pts\nPage    3 rot:   270\nFile size: 0 bytes\n";
        assert_eq!(parse_pdfinfo(out, true), PdfInfo::Pages(vec![(612.0, 792.0), (595.0, 842.0), (792.0, 612.0)]));
        assert_eq!(parse_pdfinfo("Command Line Error: Incorrect password\n", false), PdfInfo::Locked);
        assert_eq!(parse_pdfinfo("Syntax Error: Couldn't find trailer dictionary\n", false), PdfInfo::Unreadable);
        assert_eq!(parse_pdfinfo("Pages: 0\n", true), PdfInfo::Unreadable);
        // Nonsense is left out rather than believed.
        let odd = "Page 1 size: 612 x 792 pts\nPage 3 size: 1 x 1 pts\nPage 2 size: NaN x 5 pts\nPage 2 size: -4 x 5 pts\nPage 99999999 size: 5 x 5 pts\n";
        assert_eq!(parse_pdfinfo(odd, true), PdfInfo::Pages(vec![(612.0, 792.0)]));
    }

    /// Runs `script` in the jail with `input` bound at `/in/file.txt`, or on stdin if `bind` is
    /// false, and says whether it succeeded.
    fn jailed_ok(script: &str, input: Option<&Path>) -> bool {
        jailed_with(script, input, true)
    }

    fn jailed_with(script: &str, input: Option<&Path>, bind: bool) -> bool {
        let argv = [SHELL, "-c", script].map(OsString::from);
        let file = File::open(input.unwrap_or(Path::new("/dev/null"))).unwrap();
        let mut command = jailed(&argv, file, (bind && input.is_some()).then_some(".txt")).unwrap();
        command.stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap().success()
    }

    #[test]
    fn a_tool_sees_its_file_and_the_system_and_nothing_of_yours() {
        assert!(std::env::var_os("GANNET_TEST_SANDBOX").is_some(), "run tests with scripts/test.sh");
        let mine = std::env::temp_dir().join(format!("gannet-tools-{}", std::process::id()));
        std::fs::write(&mine, "the one file").unwrap();
        assert!(jailed_ok("test -x /usr/bin/sh", None), "the jail runs at all");
        assert!(jailed_ok("test \"$(cat /in/file.txt)\" = 'the one file'", Some(&mine)), "its file, bound where it's told");
        assert!(jailed_with("test \"$(cat)\" = 'the one file' && ! test -e /in", Some(&mine), false), "or on stdin, and then nowhere else");
        let home = std::env::var("HOME").unwrap();
        for (what, script) in [("home", format!("test -e '{home}'")), ("the file's own path", format!("test -e '{}'", mine.display())), ("/etc", "test -e /etc/passwd".to_string()), ("/proc", "test -e /proc/self".to_string()), ("writing /usr", "touch /usr/gannet-was-here".to_string())] {
            assert!(!jailed_ok(&script, Some(&mine)), "{what} should be out of reach");
        }
        assert!(jailed_ok("echo x > /tmp/scratch && test -s /tmp/scratch", None), "a /tmp of its own to work in");
        std::fs::remove_file(&mine).unwrap();
    }

    #[test]
    fn command_lines() {
        assert_eq!(pdftoppm(1, 800), ["/usr/bin/pdftoppm", "-f", "2", "-l", "2", "-scale-to-x", "800", "-scale-to-y", "-1", "-png", "-"].map(OsString::from));
        let line = thumbnailer(|i, o| Some(vec!["/usr/bin/t".into(), i.into(), o.into()]), ".svg").unwrap();
        assert_eq!(line, ["/usr/bin/sh", "-c", "\"$@\" && exec cat /tmp/picture.png", "sh", "/usr/bin/t", "/in/file.svg", "/tmp/picture.png"].map(OsString::from));
        assert_eq!(extension(Path::new("/a/b.SVG")), ".SVG");
        assert_eq!(extension(Path::new("/a/b.$(x)")), "");
        assert_eq!(extension(Path::new("/a/noext")), "");
    }
}
