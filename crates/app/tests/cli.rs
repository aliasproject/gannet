//! The commands for scripts, run as a script would: the real binary, with no display and no
//! D-Bus. What each command does is tested in `gannet_core::cli`; these check that the binary
//! reaches it, and that everything else on the command line still opens a window.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// A scratch home for one test, inside the sandbox scripts/test.sh makes.
struct Home(PathBuf);

impl Home {
    fn new(name: &str) -> Home {
        assert!(std::env::var_os("GANNET_TEST_SANDBOX").is_some(), "run tests with scripts/test.sh, not plain cargo test");
        let dir = std::env::temp_dir().join(format!("gannet-cli-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("work")).unwrap();
        Home(std::fs::canonicalize(dir).unwrap())
    }

    fn work(&self) -> PathBuf {
        self.0.join("work")
    }

    /// Runs gannet in the working folder, with nowhere to show a window and no bus to reach.
    fn gannet(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_gannet"))
            .args(args)
            .current_dir(self.work())
            .env_clear()
            .env("PATH", "/usr/bin")
            .env("HOME", &self.0)
            .env("XDG_DATA_HOME", self.0.join("data"))
            .env("XDG_STATE_HOME", self.0.join("state"))
            .env("XDG_CONFIG_HOME", self.0.join("config"))
            .env("XDG_CACHE_HOME", self.0.join("cache"))
            .env("DBUS_SESSION_BUS_ADDRESS", format!("unix:path={}", self.0.join("no-bus").display()))
            .env("DBUS_SYSTEM_BUS_ADDRESS", format!("unix:path={}", self.0.join("no-bus").display()))
            .output()
            .unwrap()
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn text(b: &[u8]) -> &str {
    std::str::from_utf8(b).unwrap()
}

#[test]
fn trash_and_restore_work_with_no_display() {
    let h = Home::new("round-trip");
    std::fs::write(h.work().join("a.txt"), b"keep me").unwrap();
    let list = h.gannet(&["list", "--json"]);
    assert!(list.status.success(), "{}", text(&list.stderr));
    assert!(text(&list.stdout).starts_with(r#"{"version":1,"command":"list","#), "{}", text(&list.stdout));
    assert!(text(&list.stdout).contains(r#""name":"a.txt""#));

    let trashed = h.gannet(&["trash", "a.txt"]);
    assert!(trashed.status.success(), "{}", text(&trashed.stderr));
    let in_trash = h.0.join("data/Trash/files/a.txt");
    assert_eq!(text(&trashed.stdout), format!("Moved {} to the Trash, as {}\n", h.work().join("a.txt").display(), in_trash.display()));
    assert!(in_trash.exists() && h.0.join("data/Trash/info/a.txt.trashinfo").exists());

    let listed = h.gannet(&["restore"]);
    assert!(listed.status.success());
    assert!(text(&listed.stdout).contains(&format!("  {}  [{}]\n", h.work().join("a.txt").display(), in_trash.display())), "{}", text(&listed.stdout));

    let restored = h.gannet(&["restore", "--json", "a.txt"]);
    assert!(restored.status.success(), "{}", text(&restored.stderr));
    assert!(text(&restored.stdout).starts_with(r#"{"version":1,"command":"restore","restored":[{"path":"#));
    assert_eq!(std::fs::read(h.work().join("a.txt")).unwrap(), b"keep me");
}

#[test]
fn failures_go_to_stderr_with_exit_code_1() {
    let h = Home::new("failures");
    for args in [&["info", "nope"][..], &["trash", "nope"], &["restore", "nope"], &["list", "nope"], &["list", "--bogus"]] {
        let out = h.gannet(args);
        assert_eq!(out.status.code(), Some(1), "{args:?}");
        let err = text(&out.stderr);
        assert!(err.starts_with(&format!("gannet {}: ", args[0])), "{args:?}: {err}");
    }
    // A current folder that's gone is an error, not somewhere else to aim relative paths.
    std::fs::create_dir(h.0.join("gone")).unwrap();
    let out = Command::new("sh").arg("-c").arg(format!("cd {0}/gone && rmdir {0}/gone && exec \"$0\" trash tmp", h.0.display())).arg(env!("CARGO_BIN_EXE_gannet")).output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(text(&out.stderr).starts_with("gannet trash: can't tell what the current folder is"), "{}", text(&out.stderr));
    // With --json, the error is in the output too.
    let out = h.gannet(&["info", "--json", "nope"]);
    assert_eq!(out.status.code(), Some(1));
    let want = format!("\"errors\":[{{\"path\":\"{}\",\"message\":\"No such file or directory\"}}]}}\n", h.work().join("nope").display());
    assert!(text(&out.stdout).ends_with(&want), "{}", text(&out.stdout));
}

#[test]
fn help_names_the_commands() {
    let h = Home::new("help");
    let out = h.gannet(&["--help"]);
    assert!(out.status.success());
    for c in ["gannet list", "gannet info", "gannet trash", "gannet restore"] {
        assert!(text(&out.stdout).contains(c), "{c}");
    }
    let out = h.gannet(&["trash", "--help"]);
    assert!(out.status.success());
    assert!(text(&out.stdout).starts_with("usage: gannet trash [--json] PATH...\n"));
}

/// Anything that isn't a command still means a window, which can't open here, and says so.
fn tries_for_a_window(out: &Output) -> bool {
    out.status.code() == Some(1) && out.stdout.is_empty() && text(&out.stderr).starts_with("gannet: ") && !text(&out.stderr).starts_with("gannet list")
}

#[test]
fn folders_and_files_still_open_a_window() {
    let h = Home::new("window");
    std::fs::create_dir(h.work().join("list")).unwrap();
    std::fs::write(h.work().join("info"), b"").unwrap();
    for args in [&[][..], &["./list"], &["info/"], &[h.work().join("list").to_str().unwrap()], &["--select", "info"], &["LIST"]] {
        let out = h.gannet(args);
        assert!(tries_for_a_window(&out), "{args:?}: {} {}", text(&out.stdout), text(&out.stderr));
    }
    assert!(!Path::new(&h.work().join("list")).read_dir().unwrap().any(|_| true), "nothing was written");
}
