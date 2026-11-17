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

/// `gannet --claim-show-in-folder` makes Gannet answer "Show in folder" for this user, and
/// `gannet --release-show-in-folder` undoes that.
pub const CLAIM_ARG: &str = "--claim-show-in-folder";
pub const RELEASE_ARG: &str = "--release-show-in-folder";

pub const NAME: &str = "org.freedesktop.FileManager1";
pub const PATH: &str = "/org/freedesktop/FileManager1";

/// How long the service waits for another request before it exits.
const IDLE: Duration = Duration::from_secs(10 * 60);
/// A program asking for more windows than this at once gets this many.
const MAX_WINDOWS: usize = 8;
/// At most this many URIs of a request are looked at, and this many refusals logged, so
/// a huge request can't keep the bus thread busy or flood the journal.
const MAX_URIS: usize = 256;
const MAX_REFUSALS_LOGGED: usize = 8;

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

/// The path a URI from another program names, or why it can't be shown. These come
/// from outside, so only a local `file:` URI of something that's there passes.
fn usable(uri: &str, folder: bool) -> Result<PathBuf, &'static str> {
    let path = from_file_uri(uri).ok_or("not a file: URI on this machine")?;
    // A NUL can't be in a path at all, and a newline or other control character is far
    // more likely a mangled URI than a real name.
    if path.as_os_str().as_encoded_bytes().iter().any(|&b| b < 0x20 || b == 0x7f) {
        return Err("has a control character in it");
    }
    let path = crate::app::clean(&path);
    if !path.is_absolute() {
        return Err("isn't an absolute path");
    }
    if folder {
        if !path.is_dir() {
            return Err("isn't a folder that's there");
        }
    } else if std::fs::symlink_metadata(&path).is_err() {
        return Err("isn't there");
    }
    Ok(path)
}

/// The windows a request needs, and a line for each URI left out. `folders` is for
/// ShowFolders, which shows what's in each folder; otherwise each item is shown
/// selected in its folder, one window per folder.
pub fn plan(uris: &[String], folders: bool) -> (Vec<Window>, Vec<String>) {
    let mut out: Vec<Window> = Vec::new();
    let mut refused = Vec::new();
    for uri in uris.iter().take(MAX_URIS) {
        let p = match usable(uri, folders) {
            Ok(p) => p,
            Err(why) => {
                // Kept short: a caller can send anything at all.
                let shown: String = uri.chars().take(120).collect();
                refused.push(format!("{shown:?} {why}"));
                continue;
            }
        };
        if folders {
            let w = Window::Folder(p);
            if !out.contains(&w) {
                out.push(w);
            }
            continue;
        }
        // The root has no folder to show it in, so it's shown itself.
        let Some(parent) = p.parent().map(Path::to_path_buf) else {
            let w = Window::Folder(p);
            if !out.contains(&w) {
                out.push(w);
            }
            continue;
        };
        match out.iter_mut().find(|w| matches!(w, Window::Select(items) if items[0].parent() == Some(parent.as_path()))) {
            Some(Window::Select(items)) if !items.contains(&p) => items.push(p),
            Some(_) => {}
            None => out.push(Window::Select(vec![p])),
        }
    }
    out.truncate(MAX_WINDOWS);
    (out, refused)
}

/// What gets served. Each call is answered at once, and the windows are opened by the
/// service's own thread.
struct FileManager1 {
    tx: Mutex<Sender<Vec<Window>>>,
}

impl FileManager1 {
    /// Passes on what can be shown, and refuses the call if that's nothing, so the caller
    /// isn't told it worked when nothing will open.
    fn ask(&self, uris: &[String], folders: bool) -> zbus::fdo::Result<()> {
        let (windows, refused) = plan(uris, folders);
        for r in refused.iter().take(MAX_REFUSALS_LOGGED) {
            eprintln!("gannet: left out {r}");
        }
        if refused.len() > MAX_REFUSALS_LOGGED {
            eprintln!("gannet: and {} more left out", refused.len() - MAX_REFUSALS_LOGGED);
        }
        if windows.is_empty() {
            return Err(zbus::fdo::Error::InvalidArgs("nothing to show: no URI named a local file that's there".into()));
        }
        let _ = self.tx.lock().unwrap().send(windows);
        Ok(())
    }
}

#[zbus::interface(name = "org.freedesktop.FileManager1")]
impl FileManager1 {
    fn show_folders(&self, uris: Vec<String>, _startup_id: String) -> zbus::fdo::Result<()> {
        self.ask(&uris, true)
    }

    fn show_items(&self, uris: Vec<String>, _startup_id: String) -> zbus::fdo::Result<()> {
        self.ask(&uris, false)
    }

    /// Gannet has no properties window. Showing the items instead would look to the caller
    /// like what it asked for, so it's told plainly.
    fn show_item_properties(&self, _uris: Vec<String>, _startup_id: String) -> zbus::fdo::Result<()> {
        Err(zbus::fdo::Error::NotSupported("Gannet has no properties window".into()))
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
pub(crate) fn own_path() -> PathBuf {
    let Ok(me) = std::env::current_exe() else { return PathBuf::from("gannet") };
    match me.as_os_str().as_encoded_bytes().strip_suffix(b" (deleted)") {
        // SAFETY: cut from an OsStr's own bytes at an ASCII boundary.
        Some(b) => PathBuf::from(unsafe { std::ffi::OsStr::from_encoded_bytes_unchecked(b) }),
        None => me,
    }
}

/// The first line of the service file `--claim-show-in-folder` writes, so it can tell
/// its own file from one someone else put there.
const MARK: &str = "# Written by gannet --claim-show-in-folder, and removed by gannet --release-show-in-folder.";

/// Where a user's own service file goes. D-Bus reads this folder before the system's, so
/// a file here decides who answers, whatever packages are installed.
fn user_service_file(data_home: &Path) -> PathBuf {
    data_home.join("dbus-1/services").join(format!("{NAME}.service"))
}

/// The service file's text, starting the service from `exe`. D-Bus splits `Exec` like a
/// shell, so the path is quoted.
fn service_text(exe: &Path) -> String {
    let quoted = exe.to_string_lossy().replace('\\', "\\\\").replace('"', "\\\"");
    format!("{MARK}\n[D-BUS Service]\nName={NAME}\nExec=\"{quoted}\" {ARG}\n")
}

/// Whether anything is at `path`, a broken symlink included.
fn taken(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

/// Whether Gannet wrote the file at `path`. A symlink is never Gannet's.
fn is_ours(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.is_file()) && std::fs::read_to_string(path).is_ok_and(|t| t.lines().next() == Some(MARK))
}

/// Writes the user's service file. A file there that Gannet didn't write is left alone.
fn claim(data_home: &Path, exe: &Path) -> Result<String, String> {
    let path = user_service_file(data_home);
    if taken(&path) && !is_ours(&path) {
        return Err(format!("{} is already there and wasn't written by Gannet, so it's left alone", path.display()));
    }
    let dir = path.parent().unwrap_or(data_home);
    std::fs::create_dir_all(dir).map_err(|e| format!("couldn't make {}: {e}", dir.display()))?;
    // Written beside it and moved into place, so D-Bus never reads half a file.
    let tmp = dir.join(format!(".{NAME}.service.tmp"));
    if let Err(e) = std::fs::write(&tmp, service_text(exe)).and_then(|_| std::fs::rename(&tmp, &path)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("couldn't write {}: {e}", path.display()));
    }
    Ok(format!("\"Show in folder\" opens Gannet from now on ({}). A file manager already running and answering it keeps doing so until it exits.", path.display()))
}

/// Removes the user's service file, if Gannet wrote it.
fn release(data_home: &Path) -> Result<String, String> {
    let path = user_service_file(data_home);
    if !taken(&path) {
        return Ok("Gannet wasn't answering \"Show in folder\" for this user".into());
    }
    if !is_ours(&path) {
        return Err(format!("{} wasn't written by Gannet, so it's left alone", path.display()));
    }
    std::fs::remove_file(&path).map_err(|e| format!("couldn't remove {}: {e}", path.display()))?;
    Ok(format!("\"Show in folder\" no longer opens Gannet (removed {})", path.display()))
}

/// Asks the session bus to read its service files again, so a change takes effect now.
fn reload_bus() -> Result<(), String> {
    let env = |k: &str| std::env::var(k).ok();
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
    let address = bus::session_address(env("DBUS_SESSION_BUS_ADDRESS").as_deref(), runtime.as_deref()).ok_or("no session bus")?;
    let b = Bus::connect(&address).map_err(|e| e.to_string())?;
    b.call::<_, ()>("org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus", "ReloadConfig", &()).map_err(|e| e.to_string())
}

/// `gannet --claim-show-in-folder` or `--release-show-in-folder`. Never returns.
pub fn claim_main(undo: bool) -> ! {
    let home = std::env::var_os("HOME").map(PathBuf::from).filter(|p| p.is_absolute());
    let data_home = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).filter(|p| p.is_absolute()).or_else(|| home.map(|h| h.join(".local/share")));
    // Without a home there's no right place for a per-user file, and guessing could write
    // somewhere shared.
    let Some(data_home) = data_home else {
        eprintln!("gannet: neither XDG_DATA_HOME nor HOME is set to a full path, so there's nowhere to write this");
        std::process::exit(1);
    };
    let done = if undo { release(&data_home) } else { claim(&data_home, &own_path()) };
    match done {
        Ok(msg) => {
            println!("{msg}");
            if let Err(e) = reload_bus() {
                println!("The session bus couldn't be told ({e}), so it takes effect after logging out and back in.");
            }
            std::process::exit(0);
        }
        Err(e) => {
            eprintln!("gannet: {e}");
            std::process::exit(1);
        }
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

    /// A folder of files for a test, removed after it.
    struct Files(PathBuf);

    impl Files {
        fn new(tag: &str, names: &[&str]) -> Self {
            assert!(std::env::var_os("GANNET_TEST_SANDBOX").is_some(), "run tests with scripts/test.sh");
            let root = std::env::temp_dir().join(format!("gannet-test-fm1-{}-{tag}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            for n in names {
                let p = root.join(n);
                std::fs::create_dir_all(p.parent().unwrap()).unwrap();
                if n.ends_with('/') {
                    std::fs::create_dir_all(&p).unwrap();
                } else {
                    std::fs::write(&p, b"").unwrap();
                }
            }
            Self(root)
        }

        fn path(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }

        fn uri(&self, name: &str) -> String {
            gannet_core::uri::to_file_uri(&self.0.join(name))
        }
    }

    impl Drop for Files {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn items_open_their_folder_selected_one_window_per_folder() {
        let f = Files::new("items", &["home/a.txt", "home/b c.txt", "tmp/x"]);
        let (got, refused) = plan(&[f.uri("home/a.txt"), f.uri("home/b c.txt"), f.uri("tmp/x"), f.uri("home/a.txt")], false);
        assert_eq!(got, [Window::Select(vec![f.path("home/a.txt"), f.path("home/b c.txt")]), Window::Select(vec![f.path("tmp/x")])]);
        assert!(refused.is_empty());
    }

    #[test]
    fn folders_open_themselves() {
        let f = Files::new("folders", &["dl/"]);
        let (got, _) = plan(&[format!("{}/", f.uri("dl")), f.uri("dl"), "file:///".into()], true);
        assert_eq!(got, [Window::Folder(f.path("dl")), Window::Folder("/".into())]);
    }

    #[test]
    fn only_local_things_that_are_there_count_each_refused_with_a_reason() {
        let f = Files::new("refused", &["d/good", "d/file"]);
        let bad = [
            "https://example.com/a".to_string(),
            "file://otherhost/a".into(),
            "relative".into(),
            String::new(),
            f.uri("d/missing"),
            format!("{}%0Ax", f.uri("d/good")),
            format!("{}%00x", f.uri("d/good")),
        ];
        let mut all = bad.to_vec();
        all.push(f.uri("d/good"));
        let (got, refused) = plan(&all, false);
        assert_eq!(got, [Window::Select(vec![f.path("d/good")])], "one bad URI doesn't spoil the rest");
        assert_eq!(refused.len(), bad.len(), "{refused:?}");
        // A file isn't a folder to open.
        let (got, refused) = plan(&[f.uri("d/file")], true);
        assert!(got.is_empty() && refused.len() == 1);
        // The root is shown itself, once.
        assert_eq!(plan(&["file:///".into(), "file:///".into()], false).0, [Window::Folder("/".into())]);
    }

    #[test]
    fn a_huge_request_is_only_looked_at_so_far() {
        let uris: Vec<String> = (0..10_000).map(|i| format!("file:///nonexistent/{i}")).collect();
        let (got, refused) = plan(&uris, false);
        assert!(got.is_empty());
        assert_eq!(refused.len(), MAX_URIS);
    }

    #[test]
    fn there_is_a_limit_on_windows() {
        let names: Vec<String> = (0..20).map(|i| format!("d{i}/x")).collect();
        let f = Files::new("many", &names.iter().map(String::as_str).collect::<Vec<_>>());
        let uris: Vec<String> = names.iter().map(|n| f.uri(n)).collect();
        assert_eq!(plan(&uris, false).0.len(), MAX_WINDOWS);
    }

    #[test]
    fn claiming_writes_a_file_of_ours_and_releasing_removes_only_ours() {
        let f = Files::new("claim", &[]);
        let data = f.path("data");
        let file = user_service_file(&data);
        claim(&data, Path::new("/opt/My Apps/gannet")).unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.starts_with(MARK));
        assert!(text.contains("Name=org.freedesktop.FileManager1"));
        assert!(text.contains(r#"Exec="/opt/My Apps/gannet" --file-manager-service"#), "{text}");
        // Claiming again just rewrites ours.
        claim(&data, Path::new("/usr/bin/gannet")).unwrap();
        assert!(std::fs::read_to_string(&file).unwrap().contains(r#"Exec="/usr/bin/gannet""#));
        release(&data).unwrap();
        assert!(!file.exists());
        assert!(release(&data).is_ok(), "nothing to release is fine");
        // Nor is a symlink, even a broken one.
        std::os::unix::fs::symlink("/nonexistent", &file).unwrap();
        assert!(claim(&data, Path::new("/usr/bin/gannet")).is_err());
        assert!(release(&data).is_err());
        assert!(std::fs::symlink_metadata(&file).unwrap().file_type().is_symlink());
        std::fs::remove_file(&file).unwrap();
        // Someone else's file is never touched.
        std::fs::write(&file, "[D-BUS Service]\nName=org.freedesktop.FileManager1\nExec=/usr/bin/other\n").unwrap();
        assert!(claim(&data, Path::new("/usr/bin/gannet")).is_err());
        assert!(release(&data).is_err());
        assert!(std::fs::read_to_string(&file).unwrap().contains("/usr/bin/other"));
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
        let f = Files::new("show", &["me/a.txt"]);
        let gannet = Bus::connect(&bus.address).unwrap();
        let rx = start(&gannet).unwrap().expect("the name was free");
        for (method, uri) in [("ShowItems", f.uri("me/a.txt")), ("ShowFolders", f.uri("me"))] {
            let out = busctl(&bus.address, method, &[&uri]);
            assert!(out.status.success(), "{method}: {}", String::from_utf8_lossy(&out.stderr));
        }
        let mut opened = Vec::new();
        run(&rx, Duration::from_millis(300), |w| opened.push(w.clone()));
        assert_eq!(opened, [Window::Select(vec![f.path("me/a.txt")]), Window::Folder(f.path("me"))]);
    }

    #[test]
    fn a_call_gannet_cant_honour_is_refused_not_answered_as_done() {
        let bus = PrivateBus::start("refuse");
        let f = Files::new("refuse", &["a"]);
        let gannet = Bus::connect(&bus.address).unwrap();
        let rx = start(&gannet).unwrap().expect("the name was free");
        let props = busctl(&bus.address, "ShowItemProperties", &[&f.uri("a")]);
        assert!(!props.status.success());
        assert!(String::from_utf8_lossy(&props.stderr).contains("properties"), "{}", String::from_utf8_lossy(&props.stderr));
        let nothing = busctl(&bus.address, "ShowItems", &[&f.uri("missing")]);
        assert!(!nothing.status.success());
        assert!(rx.recv_timeout(Duration::from_millis(300)).is_err(), "no window for either");
    }

    #[test]
    fn going_idle_lets_the_name_go_and_still_opens_what_was_on_its_way() {
        let bus = PrivateBus::start("finish");
        let gannet = Bus::connect(&bus.address).unwrap();
        let rx = start(&gannet).unwrap().expect("the name was free");
        let f = Files::new("finish", &["late/x"]);
        // Answered, but not yet handled when the service decides it's idle.
        assert!(busctl(&bus.address, "ShowItems", &[&f.uri("late/x")]).status.success());
        let mut opened = Vec::new();
        finish(&gannet, &rx, |w| opened.push(w.clone()));
        assert_eq!(opened, [Window::Select(vec![f.path("late/x")])]);
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
