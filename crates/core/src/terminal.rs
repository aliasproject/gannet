//! Opening a terminal in a folder, through `xdg-terminal-exec`, which starts whichever terminal
//! the person has chosen in `xdg-terminals.list`. Nothing here knows about any one terminal.
//!
//! `xdg-terminal-exec --dir=FOLDER` passes the folder with the terminal's own option where its
//! desktop entry names one (`X-TerminalArgDir`), and otherwise changes to it before starting the
//! terminal. The launcher also starts in the folder, for the few names `--dir=` can't carry.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// The program that starts the person's terminal.
pub const LAUNCHER: &str = "xdg-terminal-exec";

/// A terminal that ends badly this soon after it's asked for didn't start: the launcher and
/// systemd-run fail within a fraction of a second. One that ends later was closed, whatever its
/// exit status, and that isn't news.
const STARTING: Duration = Duration::from_secs(3);

/// How terminals are started here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Terminal {
    /// `xdg-terminal-exec`, found on PATH.
    pub launcher: PathBuf,
    /// `systemd-run`, where the session has a systemd user manager to make the terminal a
    /// scope of its own in, so it outlives Gannet and isn't counted as part of it.
    pub scope: Option<PathBuf>,
}

impl Terminal {
    /// Looks for the launcher on `path` (the value of PATH), and for `systemd-run` where a user
    /// manager runs, which it does when its socket is in `runtime_dir` (XDG_RUNTIME_DIR).
    /// `None` without the launcher: then there's no terminal to offer.
    pub fn find(path: Option<&OsStr>, runtime_dir: Option<&Path>) -> Option<Terminal> {
        let path = path?;
        let launcher = on_path(path, LAUNCHER)?;
        let manager = runtime_dir.is_some_and(|d| d.is_absolute() && d.join("systemd/private").exists());
        let scope = if manager { on_path(path, "systemd-run") } else { None };
        Some(Terminal { launcher, scope })
    }

    /// The command that opens a terminal in `dir`.
    pub fn command(&self, dir: &Path) -> Command {
        let mut c = match &self.scope {
            Some(run) => {
                let mut c = Command::new(run);
                // As app launchers start apps: a scope in app.slice, gone once the terminal is.
                c.args(["--user", "--scope", "--slice=app.slice", "--collect", "--quiet", "--"]).arg(&self.launcher);
                c
            }
            None => Command::new(&self.launcher),
        };
        // The launcher reads `--dir=` with the shell's `read`, which stops at a newline and drops
        // a trailing `=`. A folder whose name it would change is left to the folder it starts in.
        let bytes = dir.as_os_str().as_encoded_bytes();
        if !bytes.contains(&b'\n') && !bytes.ends_with(b"=") {
            let mut arg = OsString::from("--dir=");
            arg.push(dir);
            c.arg(arg);
        }
        c.current_dir(dir);
        c
    }

    /// Opens a terminal in `dir`, on its own. `on_fail` runs on a helper thread, with a reason
    /// for the user, if it couldn't start.
    pub fn open(&self, dir: &Path, on_fail: impl FnOnce(String) + Send + 'static) {
        open_within(&mut self.command(dir), STARTING, on_fail);
    }
}

/// Starts `cmd` detached, and counts it as failed only if it ends badly within `starting`.
fn open_within(cmd: &mut Command, starting: Duration, on_fail: impl FnOnce(String) + Send + 'static) {
    let asked = Instant::now();
    crate::open::spawn_program(cmd, move |why| {
        if asked.elapsed() < starting {
            on_fail(why);
        }
    });
}

/// The first executable file called `name` in the folders of `path`.
fn on_path(path: &OsStr, name: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    std::env::split_paths(path).filter(|d| d.is_absolute()).map(|d| d.join(name)).find(|p| p.metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::Scratch;
    use std::sync::mpsc::channel;

    /// A shell script at `path`. It's written by `install`, so no file descriptor for writing
    /// it is ever open in this process, where another test's fork could hold it and make
    /// running it fail with "Text file busy".
    fn program(path: &Path, script: &str) {
        let draft = path.with_extension("draft");
        std::fs::write(&draft, format!("#!/bin/sh\n{script}\n")).unwrap();
        assert!(Command::new("install").arg("-m755").arg(&draft).arg(path).status().unwrap().success());
    }

    fn args(c: &Command) -> Vec<String> {
        std::iter::once(c.get_program()).chain(c.get_args()).map(|a| a.to_string_lossy().into_owned()).collect()
    }

    #[test]
    fn it_is_offered_only_where_the_launcher_is_on_path() {
        let t = Scratch::new();
        let (empty, bin) = (t.dir("empty"), t.dir("bin"));
        let path = std::env::join_paths([&empty, &bin]).unwrap();
        assert_eq!(Terminal::find(Some(&path), None), None, "not installed");
        assert_eq!(Terminal::find(None, None), None, "no PATH at all");
        // A file by that name that can't be run isn't it.
        std::fs::write(bin.join(LAUNCHER), "").unwrap();
        assert_eq!(Terminal::find(Some(&path), None), None);
        program(&bin.join(LAUNCHER), "exit 0");
        assert_eq!(Terminal::find(Some(&path), None), Some(Terminal { launcher: bin.join(LAUNCHER), scope: None }));
        // A relative folder on PATH would depend on where Gannet happens to be.
        assert_eq!(Terminal::find(Some(OsStr::new("bin")), None), None);
    }

    #[test]
    fn it_gets_a_scope_where_a_user_manager_runs() {
        let t = Scratch::new();
        let (bin, runtime) = (t.dir("bin"), t.path().join("run"));
        t.dir("run/systemd");
        program(&bin.join(LAUNCHER), "exit 0");
        let path = bin.clone().into_os_string();
        assert_eq!(Terminal::find(Some(&path), Some(&runtime)).unwrap().scope, None, "no systemd-run");
        program(&bin.join("systemd-run"), "exit 0");
        assert_eq!(Terminal::find(Some(&path), Some(&runtime)).unwrap().scope, None, "no user manager");
        std::fs::write(runtime.join("systemd/private"), "").unwrap();
        assert_eq!(Terminal::find(Some(&path), Some(&runtime)).unwrap().scope, Some(bin.join("systemd-run")));
        assert_eq!(Terminal::find(Some(&path), None).unwrap().scope, None, "no runtime folder");
    }

    #[test]
    fn the_terminal_is_asked_for_the_folder_in_its_own_scope() {
        let dir = Path::new("/home/u/My Projects/a=b");
        let plain = Terminal { launcher: "/usr/bin/xdg-terminal-exec".into(), scope: None };
        let c = plain.command(dir);
        assert_eq!(args(&c), ["/usr/bin/xdg-terminal-exec", "--dir=/home/u/My Projects/a=b"]);
        assert_eq!(c.get_current_dir(), Some(dir));
        let scoped = Terminal { scope: Some("/usr/bin/systemd-run".into()), ..plain };
        let c = scoped.command(dir);
        assert_eq!(args(&c), ["/usr/bin/systemd-run", "--user", "--scope", "--slice=app.slice", "--collect", "--quiet", "--", "/usr/bin/xdg-terminal-exec", "--dir=/home/u/My Projects/a=b"]);
        assert_eq!(c.get_current_dir(), Some(dir));
    }

    #[test]
    fn a_folder_the_launcher_would_misread_is_left_to_the_starting_folder() {
        for dir in ["/tmp/two\nlines", "/tmp/a=", "/tmp/a=="] {
            let dir = Path::new(dir);
            let c = Terminal { launcher: "/usr/bin/xdg-terminal-exec".into(), scope: None }.command(dir);
            assert_eq!(args(&c), ["/usr/bin/xdg-terminal-exec"], "{dir:?}");
            assert_eq!(c.get_current_dir(), Some(dir));
        }
    }

    #[test]
    fn the_launcher_starts_in_the_folder_with_it_named() {
        let t = Scratch::new();
        let (bin, folder) = (t.dir("bin"), t.dir("a folder"));
        let log = t.path().join("log");
        program(&bin.join(LAUNCHER), &format!("printf '%s|%s\\n' \"$PWD\" \"$*\" > '{}'", log.display()));
        let term = Terminal::find(Some(bin.as_os_str()), None).unwrap();
        let (tx, rx) = channel();
        term.open(&folder, move |why| tx.send(why).unwrap());
        let deadline = Instant::now() + Duration::from_secs(10);
        while !log.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        std::thread::sleep(Duration::from_millis(50));
        let f = folder.display();
        assert_eq!(std::fs::read_to_string(&log).unwrap(), format!("{f}|--dir={f}\n"));
        assert!(rx.recv_timeout(Duration::from_millis(300)).is_err(), "it started");
    }

    #[test]
    fn a_terminal_that_cant_start_says_why() {
        let t = Scratch::new();
        let bin = t.dir("bin");
        program(&bin.join(LAUNCHER), "exit 1");
        let term = Terminal::find(Some(bin.as_os_str()), None).unwrap();
        let (tx, rx) = channel();
        term.open(t.path(), move |why| tx.send(why).unwrap());
        assert_eq!(rx.recv_timeout(Duration::from_secs(10)).unwrap(), "it exited with code 1");
        // A folder that's gone can't be started in.
        let (tx, rx) = channel();
        term.open(&t.path().join("gone"), move |why| tx.send(why).unwrap());
        assert!(!rx.recv_timeout(Duration::from_secs(10)).unwrap().is_empty());
    }

    #[test]
    fn a_terminal_closed_later_isnt_a_failure() {
        let (tx, rx) = channel();
        open_within(Command::new("sh").args(["-c", "sleep 0.3; exit 1"]), Duration::from_millis(100), move |why| tx.send(why).unwrap());
        assert!(rx.recv_timeout(Duration::from_secs(2)).is_err());
    }
}
