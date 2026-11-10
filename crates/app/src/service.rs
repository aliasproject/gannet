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
    let paths = uris.iter().filter_map(|u| from_file_uri(u)).map(|p| crate::app::clean(&p)).filter(|p| p.is_absolute());
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
    let me = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("gannet"));
    run(&rx, IDLE, |w| {
        gannet_core::open::spawn_detached(Command::new(&me).args(w.args()), |why| eprintln!("gannet: couldn't open a window: {why}"));
    });
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
    fn windows_get_the_arguments_that_show_them() {
        assert_eq!(Window::Folder("/a b".into()).args(), [OsString::from("/a b")]);
        assert_eq!(Window::Select(vec!["/a/x".into(), "/a/y".into()]).args(), [OsString::from("--select"), "/a/x".into(), "/a/y".into()]);
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
    fn a_second_service_leaves_the_name_to_the_first() {
        let bus = PrivateBus::start("second");
        let first = Bus::connect(&bus.address).unwrap();
        let _rx = start(&first).unwrap().expect("the name was free");
        let second = Bus::connect(&bus.address).unwrap();
        assert!(start(&second).unwrap().is_none());
    }
}
