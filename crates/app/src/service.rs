//! "Show in folder" for other programs, through `org.freedesktop.FileManager1`.
//!
//! `gannet --file-manager-service` runs without a window. It owns the name on the session
//! bus and opens an ordinary Gannet window for each request, so windows never wait on
//! the bus. With nothing asked of it for a while it exits, and D-Bus starts it again on
//! the next request, from the service file in `packaging/`.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::time::Duration;

use gannet_core::bus::{self, Bus};
use gannet_core::uri::from_file_uri;

pub const ARG: &str = "--file-manager-service";
/// `gannet --select PATH...` opens the first path's folder with the paths in it selected.
pub const SELECT_ARG: &str = "--select";

pub const NAME: &str = "org.freedesktop.FileManager1";
pub const PATH: &str = "/org/freedesktop/FileManager1";

/// How long the service waits for another request before it exits.
const IDLE: Duration = Duration::from_secs(10 * 60);
/// A program asking for more windows than this at once gets this many.
const MAX_WINDOWS: usize = 8;

/// A window to open for a request.
#[derive(Clone, Debug, PartialEq)]
pub enum Window {
    /// A folder, showing what's in it.
    Folder(PathBuf),
    /// Items that share a folder: the folder, with them selected.
    Select(Vec<PathBuf>),
}

impl Window {
    /// The arguments that make a Gannet window show this.
    pub fn args(&self) -> Vec<OsString> {
        match self {
            Window::Folder(p) => vec![p.clone().into_os_string()],
            Window::Select(items) => std::iter::once(OsString::from(SELECT_ARG)).chain(items.iter().map(|p| p.clone().into_os_string())).collect(),
        }
    }
}

/// The windows a request needs. `folders` is for ShowFolders, which shows what's in each
/// folder; otherwise each item is shown selected in its folder, one window per folder.
/// Anything that isn't a local `file:` URI of an absolute path is left out.
pub fn plan(uris: &[String], folders: bool) -> Vec<Window> {
    // A NUL can't be in a real path, and would stop the window's whole command.
    let paths = uris.iter().filter_map(|u| from_file_uri(u)).filter(|p| !p.as_os_str().as_encoded_bytes().contains(&0)).map(|p| crate::app::clean(&p)).filter(|p| p.is_absolute());
    let mut out: Vec<Window> = Vec::new();
    for p in paths {
        if folders {
            let w = Window::Folder(p);
            if !out.contains(&w) {
                out.push(w);
            }
            continue;
        }
        // The root has no folder to show it in, so it's shown itself.
        let Some(parent) = p.parent().map(Path::to_path_buf) else {
            out.push(Window::Folder(p));
            continue;
        };
        match out.iter_mut().find(|w| matches!(w, Window::Select(items) if items[0].parent() == Some(parent.as_path()))) {
            Some(Window::Select(items)) if !items.contains(&p) => items.push(p),
            Some(_) => {}
            None => out.push(Window::Select(vec![p])),
        }
    }
    out.truncate(MAX_WINDOWS);
    out
}

/// What gets served. Each call is answered at once, and the windows are opened by the
/// service's own thread.
struct FileManager1 {
    tx: Mutex<Sender<Vec<Window>>>,
}

impl FileManager1 {
    fn ask(&self, windows: Vec<Window>) {
        let _ = self.tx.lock().unwrap().send(windows);
    }
}

#[zbus::interface(name = "org.freedesktop.FileManager1")]
impl FileManager1 {
    fn show_folders(&self, uris: Vec<String>, _startup_id: String) {
        self.ask(plan(&uris, true));
    }

    fn show_items(&self, uris: Vec<String>, _startup_id: String) {
        self.ask(plan(&uris, false));
    }

    /// Gannet has no properties window, so this shows the items, as ShowItems does.
    fn show_item_properties(&self, uris: Vec<String>, _startup_id: String) {
        self.ask(plan(&uris, false));
    }
}

/// Serves on `bus` and owns the name. None if another program has the name already.
fn start(bus: &Bus) -> zbus::Result<Option<Receiver<Vec<Window>>>> {
    let (tx, rx) = channel();
    bus.serve(PATH, FileManager1 { tx: Mutex::new(tx) })?;
    Ok(bus.own_name(NAME)?.then_some(rx))
}

/// Opens windows for requests until none has come for `idle`.
fn run(rx: &Receiver<Vec<Window>>, idle: Duration, mut open: impl FnMut(&Window)) {
    loop {
        match rx.recv_timeout(idle) {
            Ok(windows) => windows.iter().for_each(&mut open),
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// Lets the name go, so D-Bus starts a new service for the next request, then opens
/// whatever arrived meanwhile. zbus answers a call before passing it on, so a request
/// can be answered and still on its way here. Waiting 200 ms for those covers all but a
/// badly overloaded machine, where one could still be lost.
fn finish(bus: &Bus, rx: &Receiver<Vec<Window>>, mut open: impl FnMut(&Window)) {
    if let Err(e) = bus.release_name(NAME) {
        eprintln!("gannet: couldn't let go of {NAME}: {e}");
    }
    std::thread::sleep(Duration::from_millis(200));
    while let Ok(windows) = rx.try_recv() {
        windows.iter().for_each(&mut open);
    }
}

/// The command that opens a window. When D-Bus starts this service, it runs in a systemd
/// unit of its own, and systemd stops everything left in that unit when the service
/// exits, so windows would close with it. There, each window gets a scope of its own in
/// `app.slice`, as app launchers do.
fn window_command(exe: &Path, w: &Window, in_unit: bool) -> Command {
    let mut c;
    if in_unit {
        c = Command::new("systemd-run");
        c.args(["--user", "--scope", "--slice=app.slice", "--collect", "--quiet", "--"]).arg(exe);
    } else {
        c = Command::new(exe);
    }
    c.args(w.args());
    c
}

/// Whether this runs in a systemd unit that systemd-run can make scopes beside.
fn in_systemd_unit() -> bool {
    std::env::var_os("INVOCATION_ID").is_some()
        && std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join("systemd-run").is_file()))
}

/// This program's path. After an upgrade replaces it while the service runs, Linux
/// reports the old file as "PATH (deleted)", and the new one is at PATH.
fn own_path() -> PathBuf {
    let Ok(me) = std::env::current_exe() else { return PathBuf::from("gannet") };
    match me.as_os_str().as_encoded_bytes().strip_suffix(b" (deleted)") {
        // SAFETY: cut from an OsStr's own bytes at an ASCII boundary.
        Some(b) => PathBuf::from(unsafe { std::ffi::OsStr::from_encoded_bytes_unchecked(b) }),
        None => me,
    }
}

/// `gannet --file-manager-service`. Never returns.
pub fn main() -> ! {
    let env = |k: &str| std::env::var(k).ok();
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
    let Some(address) = bus::session_address(env("DBUS_SESSION_BUS_ADDRESS").as_deref(), runtime.as_deref()) else {
        eprintln!("gannet: no session bus to serve \"Show in folder\" on");
        std::process::exit(1);
    };
    let bus = match Bus::connect(&address) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("gannet: couldn't connect to the session bus: {e}");
            std::process::exit(1);
        }
    };
    let rx = match start(&bus) {
        Ok(Some(rx)) => rx,
        Ok(None) => {
            eprintln!("gannet: another program already answers \"Show in folder\"");
            std::process::exit(0);
        }
        Err(e) => {
            eprintln!("gannet: couldn't serve \"Show in folder\": {e}");
            std::process::exit(1);
        }
    };
    // Windows need the compositor, which a service started by D-Bus only knows about if
    // the session passed it on. Say so where it can be found, rather than fail quietly.
    if std::env::var_os("WAYLAND_DISPLAY").is_none() {
        eprintln!("gannet: WAYLAND_DISPLAY isn't set for D-Bus services, so windows may not open; see \"Show in folder\" in the README");
    }
    let in_unit = in_systemd_unit();
    if std::env::var_os("INVOCATION_ID").is_some() && !in_unit {
        eprintln!("gannet: systemd-run isn't on PATH, so windows opened here close when this service exits");
    }
    let open = |w: &Window| {
        gannet_core::open::spawn_detached_logged(&mut window_command(&own_path(), w, in_unit), |why| eprintln!("gannet: a window from \"Show in folder\" ended early: {why}"));
    };
    run(&rx, IDLE, open);
    finish(&bus, &rx, open);
    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::process::{Child, Stdio};

    fn uris(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|p| p.to_string()).collect()
    }

    #[test]
    fn items_open_their_folder_selected_one_window_per_folder() {
        let got = plan(&uris(&["file:///home/me/a.txt", "file:///home/me/b%20c.txt", "file:///tmp/x", "file:///home/me/a.txt"]), false);
        assert_eq!(got, [Window::Select(vec!["/home/me/a.txt".into(), "/home/me/b c.txt".into()]), Window::Select(vec!["/tmp/x".into()])]);
    }

    #[test]
    fn folders_open_themselves() {
        let got = plan(&uris(&["file:///home/me/Downloads/", "file:///home/me/Downloads", "file:///"]), true);
        assert_eq!(got, [Window::Folder("/home/me/Downloads".into()), Window::Folder("/".into())]);
    }

    #[test]
    fn only_local_files_count_and_there_is_a_limit() {
        assert_eq!(plan(&uris(&["https://example.com/a", "file://otherhost/a", "relative", ""]), false), []);
        assert_eq!(plan(&uris(&["file:///"]), false), [Window::Folder("/".into())]);
        let many: Vec<String> = (0..50).map(|i| format!("file:///d{i}/x")).collect();
        assert_eq!(plan(&many, false).len(), MAX_WINDOWS);
        assert_eq!(plan(&uris(&["file:///a/../etc/x"]), false), [Window::Select(vec!["/etc/x".into()])]);
    }

    #[test]
    fn a_path_with_a_nul_is_left_out_without_spoiling_the_rest() {
        assert_eq!(plan(&uris(&["file:///d/bad%00x", "file:///d/good"]), false), [Window::Select(vec!["/d/good".into()])]);
    }

    #[test]
    fn windows_get_the_arguments_that_show_them() {
        assert_eq!(Window::Folder("/a b".into()).args(), [OsString::from("/a b")]);
        assert_eq!(Window::Select(vec!["/a/x".into(), "/a/y".into()]).args(), [OsString::from("--select"), "/a/x".into(), "/a/y".into()]);
    }

    #[test]
    fn inside_a_systemd_unit_windows_get_their_own_scope() {
        let w = Window::Select(vec!["/d/a".into()]);
        let args = |c: &Command| std::iter::once(c.get_program().to_owned()).chain(c.get_args().map(|a| a.to_owned())).collect::<Vec<OsString>>();
        assert_eq!(args(&window_command(Path::new("/usr/bin/gannet"), &w, false)), ["/usr/bin/gannet", "--select", "/d/a"].map(OsString::from));
        assert_eq!(
            args(&window_command(Path::new("/usr/bin/gannet"), &w, true)),
            ["systemd-run", "--user", "--scope", "--slice=app.slice", "--collect", "--quiet", "--", "/usr/bin/gannet", "--select", "/d/a"].map(OsString::from)
        );
    }

    /// A bus of our own, so tests never touch the real session bus.
    struct PrivateBus {
        daemon: Child,
        address: String,
        dir: PathBuf,
    }

    impl PrivateBus {
        fn start(tag: &str) -> Self {
            assert!(std::env::var_os("GANNET_TEST_SANDBOX").is_some(), "run tests with scripts/test.sh");
            let dir = std::env::temp_dir().join(format!("gannet-test-bus-{}-{tag}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let config = dir.join("bus.conf");
            std::fs::write(&config, format!(
                r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:path={}</listen>
  <auth>EXTERNAL</auth>
  <policy context="default"><allow send_destination="*" eavesdrop="true"/><allow eavesdrop="true"/><allow own="*"/></policy>
</busconfig>
"#,
                dir.join("bus").display()
            ))
            .unwrap();
            let daemon = Command::new("dbus-daemon").args(["--nofork", "--print-address=1", &format!("--config-file={}", config.display())]).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().expect("dbus-daemon");
            let mut bus = Self { daemon, address: String::new(), dir };
            let mut line = String::new();
            BufReader::new(bus.daemon.stdout.take().unwrap()).read_line(&mut line).unwrap();
            bus.address = line.trim().to_string();
            assert!(!bus.address.is_empty());
            bus
        }
    }

    impl Drop for PrivateBus {
        fn drop(&mut self) {
            let _ = self.daemon.kill();
            let _ = self.daemon.wait();
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn busctl(address: &str, method: &str, uris: &[&str]) -> std::process::Output {
        let mut args = vec![format!("--address={address}"), "call".into(), NAME.into(), PATH.into(), NAME.into(), method.into(), "ass".into(), uris.len().to_string()];
        args.extend(uris.iter().map(|u| u.to_string()));
        args.push("token".into());
        Command::new("busctl").args(&args).output().expect("busctl")
    }

    #[test]
    fn another_program_asking_to_show_items_gets_windows_opened() {
        let bus = PrivateBus::start("show");
        let gannet = Bus::connect(&bus.address).unwrap();
        let rx = start(&gannet).unwrap().expect("the name was free");
        for (method, uris) in [("ShowItems", vec!["file:///home/me/a.txt"]), ("ShowFolders", vec!["file:///home/me"]), ("ShowItemProperties", vec!["file:///x/y"])] {
            let out = busctl(&bus.address, method, &uris);
            assert!(out.status.success(), "{method}: {}", String::from_utf8_lossy(&out.stderr));
        }
        let mut opened = Vec::new();
        run(&rx, Duration::from_millis(300), |w| opened.push(w.clone()));
        assert_eq!(opened, [Window::Select(vec!["/home/me/a.txt".into()]), Window::Folder("/home/me".into()), Window::Select(vec!["/x/y".into()])]);
    }

    #[test]
    fn going_idle_lets_the_name_go_and_still_opens_what_was_on_its_way() {
        let bus = PrivateBus::start("finish");
        let gannet = Bus::connect(&bus.address).unwrap();
        let rx = start(&gannet).unwrap().expect("the name was free");
        // Answered, but not yet handled when the service decides it's idle.
        assert!(busctl(&bus.address, "ShowItems", &["file:///late/x"]).status.success());
        let mut opened = Vec::new();
        finish(&gannet, &rx, |w| opened.push(w.clone()));
        assert_eq!(opened, [Window::Select(vec!["/late/x".into()])]);
        // The next request starts a new service rather than reaching this one.
        let next = Bus::connect(&bus.address).unwrap();
        assert!(start(&next).unwrap().is_some());
    }

    #[test]
    fn a_second_service_leaves_the_name_to_the_first() {
        let bus = PrivateBus::start("second");
        let first = Bus::connect(&bus.address).unwrap();
        let _rx = start(&first).unwrap().expect("the name was free");
        let second = Bus::connect(&bus.address).unwrap();
        assert!(start(&second).unwrap().is_none());
    }
}
