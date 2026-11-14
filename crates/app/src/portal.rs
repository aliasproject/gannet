//! Open and Save dialogs for other programs, through xdg-desktop-portal.
//!
//! `gannet --file-chooser-service` runs without a window. It owns
//! `org.freedesktop.impl.portal.desktop.gannet` and serves the portal's FileChooser backend.
//! Each request starts a chooser window, `gannet --choose ...`, waits for it without holding
//! up other calls, and answers with what it picked. The window belongs to the request, so it
//! stays in the service's own process group: closing the request, or the service ending,
//! takes it along. With nothing asked of it for a while the service exits, and D-Bus starts
//! it again on the next request, from the service file in `packaging/`.

use std::collections::HashMap;
use std::future::poll_fn;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::task::{Poll, Waker};
use std::time::Duration;

use gannet_core::bus::{self, Bus};
use rustix::fd::OwnedFd;
use zbus::{OwnedObjectPath, OwnedVariant as OwnedValue, Variant as Value};

use crate::chooser::{Filter, Mode, Pattern, Spec};

pub const ARG: &str = "--file-chooser-service";
pub const NAME: &str = "org.freedesktop.impl.portal.desktop.gannet";
pub const PATH: &str = "/org/freedesktop/portal/desktop";

/// How long the service waits, with no dialog open, before it exits.
const IDLE: Duration = Duration::from_secs(10 * 60);
/// Requests come from other programs, so what's taken from one is kept within reason.
const MAX_FILTERS: usize = 64;
const MAX_PATTERNS: usize = 256;
const MAX_TEXT: usize = 512;
const MAX_NAMES: usize = 1024;
/// More output than a chooser could mean is a broken one.
const MAX_OUTPUT: u64 = 16 * 1024 * 1024;

/// The portal's answers: picked, cancelled, or something else ended it.
const PICKED: u32 = 0;
const CANCELLED: u32 = 1;
const ENDED: u32 = 2;

type Options = HashMap<String, OwnedValue>;
type Results = HashMap<String, OwnedValue>;
/// A filter as the portal sends it: a label, then (0, glob) or (1, MIME type) pairs.
type WireFilter = (String, Vec<(u32, String)>);

/// One option, if it's there and of the right type.
fn opt<T: TryFrom<OwnedValue>>(o: &Options, key: &str) -> Option<T> {
    o.get(key)?.try_clone().ok()?.try_into().ok()
}

/// Text from another program, kept to a sensible length, on one line.
fn short(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).take(MAX_TEXT).collect()
}

/// A byte-string path as the portal sends it, ending in a NUL.
fn byte_path(bytes: Vec<u8>) -> Option<PathBuf> {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    let bytes = bytes[..end].to_vec();
    (!bytes.is_empty()).then(|| PathBuf::from(<std::ffi::OsString as std::os::unix::ffi::OsStringExt>::from_vec(bytes)))
}

fn filter_from(w: WireFilter) -> Filter {
    let patterns = w.1.into_iter().take(MAX_PATTERNS).filter_map(|(kind, p)| match kind {
        0 => Some(Pattern::Glob(short(&p))),
        1 => Some(Pattern::Mime(short(&p))),
        _ => None,
    });
    Filter { label: short(&w.0), patterns: patterns.collect() }
}

fn filter_to_wire(f: &Filter) -> WireFilter {
    let patterns = f.patterns.iter().map(|p| match p {
        Pattern::Glob(g) => (0, g.clone()),
        Pattern::Mime(m) => (1, m.clone()),
    });
    (f.label.clone(), patterns.collect())
}

/// What a portal request asks for, as a chooser window understands it.
pub fn spec_from(method: Method, parent_window: &str, title: &str, o: &Options) -> Spec {
    let directory = opt::<bool>(o, "directory").unwrap_or(false);
    let mode = match method {
        Method::Open if directory => Mode::Folder,
        Method::Open => Mode::Open,
        Method::Save => Mode::Save,
        Method::SaveFiles => Mode::SaveFiles,
    };
    let mut filters: Vec<Filter> = opt::<Vec<WireFilter>>(o, "filters").unwrap_or_default().into_iter().take(MAX_FILTERS).map(filter_from).collect();
    // The filter to start with is one of the list, or with no list, the only one there is.
    let mut current = None;
    if let Some(c) = opt::<WireFilter>(o, "current_filter").map(filter_from) {
        current = Some(filters.iter().position(|f| *f == c).unwrap_or_else(|| {
            filters.push(c);
            filters.len() - 1
        }));
    }
    let absolute = |p: PathBuf| p.is_absolute().then_some(p);
    let names = opt::<Vec<Vec<u8>>>(o, "files").unwrap_or_default();
    Spec {
        mode,
        multiple: mode != Mode::Save && opt::<bool>(o, "multiple").unwrap_or(false),
        title: short(title),
        accept: opt::<String>(o, "accept_label").map(|l| short(&l)),
        folder: opt::<Vec<u8>>(o, "current_folder").and_then(byte_path).and_then(absolute),
        name: opt::<String>(o, "current_name").map(|n| short(&n)).filter(|n| !n.is_empty()),
        file: opt::<Vec<u8>>(o, "current_file").and_then(byte_path).and_then(absolute),
        // Names only: a name with a folder in it would save somewhere not chosen.
        save_names: names.into_iter().take(MAX_NAMES).filter_map(byte_path).filter_map(|p| p.file_name().map(PathBuf::from)).collect(),
        filters,
        current,
        parent: Some(short(parent_window)).filter(|p| p.starts_with("wayland:")),
        modal: opt::<bool>(o, "modal").unwrap_or(true),
    }
}

/// What the portal answers with, from what the window said.
fn results(spec: &Spec, uris: Vec<String>, filter: Option<usize>, o: &Options) -> Results {
    let mut r = Results::new();
    let mut put = |k: &str, v: Value<'_>| {
        if let Ok(v) = OwnedValue::try_from(v) {
            r.insert(k.to_string(), v);
        }
    };
    put("uris", Value::from(uris));
    if let Some(f) = filter.and_then(|i| spec.filters.get(i)) {
        put("current_filter", Value::from(filter_to_wire(f)));
    }
    // Gannet doesn't show a request's extra choices, so it answers with each one's default.
    type Choice = (String, String, Vec<(String, String)>, String);
    let choices: Vec<(String, String)> = opt::<Vec<Choice>>(o, "choices").unwrap_or_default().into_iter().map(|(id, _, options, initial)| {
        let value = if initial.is_empty() { options.first().map(|o| o.0.clone()).unwrap_or_default() } else { initial };
        (id, value)
    }).collect();
    if !choices.is_empty() {
        put("choices", Value::from(choices));
    }
    r
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Method {
    Open,
    Save,
    SaveFiles,
}

/// A value one thread hands to a task waiting for it. If the sender goes without sending,
/// the waiter gets `None`, so it can never be left waiting.
struct Slot<T> {
    state: Mutex<(Option<Option<T>>, Option<Waker>)>,
}

struct Handing<T>(Arc<Slot<T>>, bool);

impl<T> Handing<T> {
    fn send(mut self, v: T) {
        self.put(Some(v));
    }

    fn put(&mut self, v: Option<T>) {
        self.1 = true;
        let mut s = self.0.state.lock().unwrap();
        s.0 = Some(v);
        if let Some(w) = s.1.take() {
            w.wake();
        }
    }
}

impl<T> Drop for Handing<T> {
    fn drop(&mut self) {
        if !self.1 {
            self.put(None);
        }
    }
}

fn hand_over<T>() -> (Handing<T>, Arc<Slot<T>>) {
    let slot = Arc::new(Slot { state: Mutex::new((None, None)) });
    (Handing(slot.clone(), false), slot)
}

async fn wait_for<T>(slot: Arc<Slot<T>>) -> Option<T> {
    poll_fn(|cx| {
        let mut s = slot.state.lock().unwrap();
        match s.0.take() {
            Some(v) => Poll::Ready(v),
            None => {
                s.1 = Some(cx.waker().clone());
                Poll::Pending
            }
        }
    })
    .await
}

/// What's going on, for knowing when the service can go.
#[derive(Default)]
struct Busy {
    open: AtomicUsize,
    nudge: Mutex<Option<Sender<()>>>,
}

impl Busy {
    fn nudge(&self) {
        if let Some(tx) = self.nudge.lock().unwrap().as_ref() {
            let _ = tx.send(());
        }
    }
}

/// The portal's Request object for one dialog, which the portal closes when the program that
/// asked no longer wants an answer.
struct Request {
    window: Arc<Mutex<Option<OwnedFd>>>,
    closed: Arc<AtomicBool>,
}

#[zbus::interface(name = "org.freedesktop.impl.portal.Request")]
impl Request {
    fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        if let Some(fd) = self.window.lock().unwrap().as_ref() {
            let _ = rustix::process::pidfd_send_signal(fd, rustix::process::Signal::TERM);
        }
    }
}

struct FileChooser {
    exe: PathBuf,
    busy: Arc<Busy>,
}

impl FileChooser {
    async fn choose(&self, conn: &zbus::Connection, handle: OwnedObjectPath, method: Method, parent_window: &str, title: &str, options: Options) -> (u32, Results) {
        self.busy.open.fetch_add(1, Ordering::SeqCst);
        self.busy.nudge();
        let answer = self.run(conn, handle, method, parent_window, title, &options).await;
        self.busy.open.fetch_sub(1, Ordering::SeqCst);
        self.busy.nudge();
        answer
    }

    async fn run(&self, conn: &zbus::Connection, handle: OwnedObjectPath, method: Method, parent_window: &str, title: &str, options: &Options) -> (u32, Results) {
        let spec = spec_from(method, parent_window, title, options);
        let (window, closed) = (Arc::new(Mutex::new(None)), Arc::new(AtomicBool::new(false)));
        let served = conn.object_server().at(&handle, Request { window: window.clone(), closed: closed.clone() }).await;
        let outcome = self.window(&spec, &window, &closed).await;
        if served.is_ok() {
            let _ = conn.object_server().remove::<Request, _>(&handle).await;
        }
        window.lock().unwrap().take();
        let Some((text, ended_well)) = outcome.filter(|_| !closed.load(Ordering::SeqCst)) else { return (ENDED, Results::new()) };
        match crate::chooser::read_outcome(&text) {
            Some((uris, filter)) => (PICKED, results(&spec, uris, filter, options)),
            None if ended_well => (CANCELLED, Results::new()),
            None => (ENDED, Results::new()),
        }
    }

    /// Starts the window and waits for what it says, and whether it ended well.
    async fn window(&self, spec: &Spec, window: &Mutex<Option<OwnedFd>>, closed: &AtomicBool) -> Option<(String, bool)> {
        let mut child = match Command::new(&self.exe).args(spec.args()).stdin(Stdio::null()).stdout(Stdio::piped()).spawn() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("gannet: couldn't open a file chooser: {e}");
                return None;
            }
        };
        // A pidfd can't reach another process that later gets the same number.
        let pid = rustix::process::Pid::from_child(&child);
        *window.lock().unwrap() = rustix::process::pidfd_open(pid, rustix::process::PidfdFlags::empty()).ok();
        // Closed before the window existed to be told.
        if closed.load(Ordering::SeqCst) {
            let _ = child.kill();
        }
        let (tx, rx) = hand_over();
        let mut out = child.stdout.take()?;
        let waiting = std::thread::Builder::new().name("gannet-chooser".into()).spawn(move || {
            let mut text = String::new();
            let _ = (&mut out).take(MAX_OUTPUT).read_to_string(&mut text);
            drop(out);
            let ok = child.wait().is_ok_and(|s| s.success());
            tx.send((text, ok));
        });
        if let Err(e) = waiting {
            eprintln!("gannet: couldn't wait for the file chooser: {e}");
            return None;
        }
        wait_for(rx).await
    }
}

#[zbus::interface(name = "org.freedesktop.impl.portal.FileChooser")]
impl FileChooser {
    #[zbus(out_args("response", "results"))]
    async fn open_file(&self, #[zbus(connection)] conn: &zbus::Connection, handle: OwnedObjectPath, _app_id: String, parent_window: String, title: String, options: Options) -> (u32, Results) {
        self.choose(conn, handle, Method::Open, &parent_window, &title, options).await
    }

    #[zbus(out_args("response", "results"))]
    async fn save_file(&self, #[zbus(connection)] conn: &zbus::Connection, handle: OwnedObjectPath, _app_id: String, parent_window: String, title: String, options: Options) -> (u32, Results) {
        self.choose(conn, handle, Method::Save, &parent_window, &title, options).await
    }

    #[zbus(out_args("response", "results"))]
    async fn save_files(&self, #[zbus(connection)] conn: &zbus::Connection, handle: OwnedObjectPath, _app_id: String, parent_window: String, title: String, options: Options) -> (u32, Results) {
        self.choose(conn, handle, Method::SaveFiles, &parent_window, &title, options).await
    }
}

/// Serves on `bus` and owns the name. None if another program has the name already.
fn start(bus: &Bus, exe: &Path) -> zbus::Result<Option<(Arc<Busy>, Receiver<()>)>> {
    let (tx, rx) = channel();
    let busy = Arc::new(Busy { open: AtomicUsize::new(0), nudge: Mutex::new(Some(tx)) });
    bus.serve(PATH, FileChooser { exe: exe.to_path_buf(), busy: busy.clone() })?;
    Ok(bus.own_name(NAME)?.then_some((busy, rx)))
}

/// Waits until no dialog has been open for `idle`.
fn run(busy: &Busy, rx: &Receiver<()>, idle: Duration) {
    loop {
        match rx.recv_timeout(idle) {
            Ok(()) => {}
            Err(RecvTimeoutError::Timeout) if busy.open.load(Ordering::SeqCst) == 0 => return,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// Lets the name go, so D-Bus starts a new service for the next request, then waits for any
/// dialog that was asked for meanwhile.
fn finish(bus: &Bus, busy: &Busy) {
    if let Err(e) = bus.release_name(NAME) {
        eprintln!("gannet: couldn't let go of {NAME}: {e}");
    }
    std::thread::sleep(Duration::from_millis(200));
    while busy.open.load(Ordering::SeqCst) > 0 {
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// `gannet --file-chooser-service`. Never returns.
pub fn main() -> ! {
    let env = |k: &str| std::env::var(k).ok();
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
    let Some(address) = bus::session_address(env("DBUS_SESSION_BUS_ADDRESS").as_deref(), runtime.as_deref()) else {
        eprintln!("gannet: no session bus to serve Open and Save dialogs on");
        std::process::exit(1);
    };
    let bus = match Bus::connect(&address) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("gannet: couldn't connect to the session bus: {e}");
            std::process::exit(1);
        }
    };
    let (busy, rx) = match start(&bus, &crate::service::own_path()) {
        Ok(Some(s)) => s,
        Ok(None) => {
            eprintln!("gannet: another program already serves Gannet's Open and Save dialogs");
            std::process::exit(0);
        }
        Err(e) => {
            eprintln!("gannet: couldn't serve Open and Save dialogs: {e}");
            std::process::exit(1);
        }
    };
    if std::env::var_os("WAYLAND_DISPLAY").is_none() {
        eprintln!("gannet: WAYLAND_DISPLAY isn't set for D-Bus services, so dialogs may not open; see \"Open and Save dialogs\" in the README");
    }
    run(&busy, &rx, IDLE);
    finish(&bus, &busy);
    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::process::Child;

    fn options(pairs: Vec<(&str, Value<'_>)>) -> Options {
        pairs.into_iter().map(|(k, v)| (k.to_string(), OwnedValue::try_from(v).unwrap())).collect()
    }

    fn wire(label: &str, p: &[(u32, &str)]) -> Value<'static> {
        Value::from((label.to_string(), p.iter().map(|(k, s)| (*k, s.to_string())).collect::<Vec<_>>()))
    }

    #[test]
    fn a_request_becomes_a_spec() {
        let o = options(vec![
            ("multiple", Value::from(true)),
            ("accept_label", Value::from("_Upload")),
            ("filters", Value::from(vec![("Images".to_string(), vec![(0u32, "*.png".to_string()), (1, "image/jpeg".to_string())]), ("All".to_string(), vec![(0, "*".to_string())])])),
            ("current_filter", wire("All", &[(0, "*")])),
            ("current_folder", Value::from(b"/tmp\0".to_vec())),
        ]);
        let s = spec_from(Method::Open, "wayland:xyz", "Pick\nsome", &o);
        assert_eq!(s.mode, Mode::Open);
        assert!(s.multiple && s.modal, "modal unless asked otherwise");
        assert_eq!(s.title, "Picksome", "on one line");
        assert_eq!(s.accept.as_deref(), Some("_Upload"));
        assert_eq!(s.folder.as_deref(), Some(Path::new("/tmp")));
        assert_eq!(s.filters.len(), 2);
        assert_eq!(s.filters[0].patterns, [Pattern::Glob("*.png".into()), Pattern::Mime("image/jpeg".into())]);
        assert_eq!(s.current, Some(1));
        assert_eq!(s.parent.as_deref(), Some("wayland:xyz"));
        // X11 parents are no use to a Wayland window.
        assert_eq!(spec_from(Method::Open, "x11:12", "", &Options::new()).parent, None);
        assert_eq!(spec_from(Method::Open, "", "", &options(vec![("directory", Value::from(true))])).mode, Mode::Folder);
    }

    #[test]
    fn a_filter_given_alone_is_used_and_returned() {
        let o = options(vec![("current_filter", wire("Text", &[(1, "text/plain")]))]);
        let s = spec_from(Method::Save, "", "", &o);
        assert_eq!(s.filters, [Filter { label: "Text".into(), patterns: vec![Pattern::Mime("text/plain".into())] }]);
        assert_eq!(s.current, Some(0));
        let r = results(&s, vec!["file:///a".into()], Some(0), &o);
        let back: WireFilter = r["current_filter"].try_clone().unwrap().try_into().unwrap();
        assert_eq!(back, ("Text".to_string(), vec![(1, "text/plain".to_string())]));
    }

    #[test]
    fn saving_takes_a_name_or_a_file_and_names_only_for_several() {
        let o = options(vec![("current_name", Value::from("photo.png")), ("current_file", Value::from(b"relative/x\0".to_vec())), ("multiple", Value::from(true))]);
        let s = spec_from(Method::Save, "", "Save", &o);
        assert_eq!((s.mode, s.name.as_deref(), s.file.as_deref(), s.multiple), (Mode::Save, Some("photo.png"), None, false), "a relative path is no use, and one file is saved");
        let o = options(vec![("files", Value::from(vec![b"a.txt\0".to_vec(), b"../../etc/b\0".to_vec(), b"\0".to_vec()]))]);
        assert_eq!(spec_from(Method::SaveFiles, "", "", &o).save_names, [PathBuf::from("a.txt"), PathBuf::from("b")]);
    }

    #[test]
    fn choices_are_answered_with_their_defaults() {
        type Choice = (String, String, Vec<(String, String)>, String);
        let c: Vec<Choice> = vec![("enc".into(), "Encoding".into(), vec![("utf8".into(), "UTF-8".into())], String::new()), ("ro".into(), "Read only".into(), vec![], "true".into())];
        let o = options(vec![("choices", Value::from(c))]);
        let r = results(&spec_from(Method::Open, "", "", &o), vec!["file:///x".into()], None, &o);
        let got: Vec<(String, String)> = r["choices"].try_clone().unwrap().try_into().unwrap();
        assert_eq!(got, [("enc".to_string(), "utf8".to_string()), ("ro".to_string(), "true".to_string())]);
        assert!(!r.contains_key("current_filter"));
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
            let dir = std::env::temp_dir().join(format!("gannet-test-portal-{}-{tag}", std::process::id()));
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
            bus
        }

        /// A stand-in for the chooser window: a script that says `text` after `wait` seconds,
        /// and writes its arguments beside it.
        fn window(&self, text: &str, wait: &str) -> PathBuf {
            let p = self.dir.join("window.sh");
            let (args, said) = (self.dir.join("args"), self.dir.join("says"));
            std::fs::write(&said, text).unwrap();
            // A window that waits is the sleep itself, so a signal reaches it.
            let end = if wait == "0" { format!("cat '{}'", said.display()) } else { format!("exec sleep {wait}") };
            std::fs::write(&p, format!("#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n{end}\n", args.display())).unwrap();
            Command::new("chmod").arg("755").arg(&p).status().unwrap();
            p
        }
    }

    impl Drop for PrivateBus {
        fn drop(&mut self) {
            let _ = self.daemon.kill();
            let _ = self.daemon.wait();
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn busctl(address: &str, args: &[&str]) -> std::process::Output {
        let mut a = vec![format!("--address={address}"), "--timeout=20".into()];
        a.extend(args.iter().map(|s| s.to_string()));
        Command::new("busctl").args(&a).output().expect("busctl")
    }

    const IFACE: &str = "org.freedesktop.impl.portal.FileChooser";

    #[test]
    fn a_pick_is_answered_with_its_uris_and_filter() {
        let bus = PrivateBus::start("pick");
        let exe = bus.window("accept\nfilter 0\nuri file:///tmp/a%20b.png\nuri file:///tmp/c.png\n", "0");
        let gannet = Bus::connect(&bus.address).unwrap();
        start(&gannet, &exe).unwrap().expect("the name was free");
        let out = busctl(&bus.address, &["--json=short", "call", NAME, PATH, IFACE, "OpenFile", "osssa{sv}", "/org/freedesktop/portal/desktop/request/1_1/t", "app", "wayland:h1", "Pick", "2", "multiple", "b", "true", "filters", "a(sa(us))", "1", "Images", "1", "0", "*.png"]);
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(text.contains(r#""data":[0,"#), "{text}");
        assert!(text.contains("file:///tmp/a%20b.png") && text.contains("file:///tmp/c.png") && text.contains("current_filter"), "{text}");
        let args = std::fs::read_to_string(bus.dir.join("args")).unwrap();
        assert!(args.starts_with("--choose\nopen\n") && args.contains("--multiple") && args.contains("--parent\nwayland:h1") && args.contains("--glob\n*.png"), "{args}");
    }

    #[test]
    fn a_cancel_or_a_crash_is_answered_as_one() {
        let bus = PrivateBus::start("cancel");
        let gannet = Bus::connect(&bus.address).unwrap();
        let exe = bus.window("cancel\n", "0");
        start(&gannet, &exe).unwrap().expect("the name was free");
        let call = |method: &str| busctl(&bus.address, &["call", NAME, PATH, IFACE, method, "osssa{sv}", "/r/1", "", "", "", "0"]);
        let out = call("SaveFile");
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert!(String::from_utf8_lossy(&out.stdout).starts_with("ua{sv} 1 0"), "{}", String::from_utf8_lossy(&out.stdout));
        // A window that dies says nothing, which isn't a cancel.
        std::fs::write(&exe, "#!/bin/sh\nkill -9 $$\n").unwrap();
        let out = call("OpenFile");
        assert!(String::from_utf8_lossy(&out.stdout).starts_with("ua{sv} 2 0"), "{}", String::from_utf8_lossy(&out.stdout));
    }

    #[test]
    fn closing_the_request_closes_the_window_and_other_calls_go_on_meanwhile() {
        let bus = PrivateBus::start("close");
        let gannet = Bus::connect(&bus.address).unwrap();
        let exe = bus.window("accept\nuri file:///late\n", "30");
        let (busy, _rx) = start(&gannet, &exe).unwrap().expect("the name was free");
        let address = bus.address.clone();
        let waiting = std::thread::spawn(move || busctl(&address, &["call", NAME, PATH, IFACE, "OpenFile", "osssa{sv}", "/r/2", "", "", "", "0"]));
        let t0 = std::time::Instant::now();
        while busy.open.load(Ordering::SeqCst) == 0 || !bus.dir.join("args").exists() {
            assert!(t0.elapsed() < Duration::from_secs(10), "the window never started");
            std::thread::sleep(Duration::from_millis(20));
        }
        // The first dialog is still open, and the service still answers.
        let intro = busctl(&bus.address, &["introspect", NAME, "/r/2"]);
        assert!(String::from_utf8_lossy(&intro.stdout).contains("org.freedesktop.impl.portal.Request"), "{}", String::from_utf8_lossy(&intro.stdout));
        let closed = busctl(&bus.address, &["call", NAME, "/r/2", "org.freedesktop.impl.portal.Request", "Close"]);
        assert!(closed.status.success(), "{}", String::from_utf8_lossy(&closed.stderr));
        let out = waiting.join().unwrap();
        assert!(String::from_utf8_lossy(&out.stdout).starts_with("ua{sv} 2 0"), "{}", String::from_utf8_lossy(&out.stdout));
        assert!(t0.elapsed() < Duration::from_secs(20), "the window was stopped, not waited out");
        let t1 = std::time::Instant::now();
        while busy.open.load(Ordering::SeqCst) != 0 {
            assert!(t1.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(20));
        }
        let gone = busctl(&bus.address, &["introspect", NAME, "/r/2"]);
        assert!(!String::from_utf8_lossy(&gone.stdout).contains("portal.Request"), "the Request object goes with it");
    }

    #[test]
    fn the_service_waits_out_open_dialogs_before_it_goes() {
        let busy = Busy::default();
        let (tx, rx) = channel();
        *busy.nudge.lock().unwrap() = Some(tx);
        busy.open.store(1, Ordering::SeqCst);
        let t0 = std::time::Instant::now();
        std::thread::scope(|s| {
            s.spawn(|| {
                std::thread::sleep(Duration::from_millis(150));
                busy.open.store(0, Ordering::SeqCst);
            });
            run(&busy, &rx, Duration::from_millis(50));
        });
        assert!(t0.elapsed() >= Duration::from_millis(150));
    }
}
