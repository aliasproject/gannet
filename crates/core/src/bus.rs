//! D-Bus, through zbus.
//!
//! zbus is async, and Gannet has no async runtime. Connections run on zbus's own helper
//! thread, which also answers calls to objects served here, so serving needs nothing from
//! Gannet's main loop. Served objects reach the UI by sending to the app's message hub.
//!
//! Every `Bus` method blocks its thread until the bus answers, with a time limit, so call
//! them from a worker thread, never the UI thread. Never call them from inside a served
//! object's method either: that runs on zbus's thread, where blocking on zbus panics. A
//! method that has to wait, like a file chooser waiting for the user's pick, must be an
//! `async fn` that awaits the answer, or it stalls every other call and signal.
//!
//! Like the rest of this crate, nothing here reads the environment. The app works out
//! the addresses with `session_address` and `system_address` and passes them in.

use std::future::{Future, poll_fn};
use std::path::Path;
use std::pin::{Pin, pin};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::task::{Poll, Waker};
use std::thread;
use std::time::Duration;

use futures_core::Stream;
use zbus::fdo::{RequestNameFlags, RequestNameReply};
use zbus::object_server::Interface;
use zbus::wire::{DynamicDeserialize, DynamicType};
use zbus::{Connection, MatchRule, Message, MessageStream, block_on};

pub use zbus;

/// How long to wait for the bus, or a program on it, before giving up. D-Bus's own
/// default for a method call.
pub const TIME_LIMIT: Duration = Duration::from_secs(25);

/// Where the session bus is: `DBUS_SESSION_BUS_ADDRESS` if it's set, or else the
/// `bus` socket in `XDG_RUNTIME_DIR`, which is where systemd's user bus lives.
pub fn session_address(env: Option<&str>, runtime_dir: Option<&Path>) -> Option<String> {
    if let Some(a) = env.filter(|a| !a.is_empty()) {
        return Some(a.to_string());
    }
    let socket = runtime_dir?.join("bus");
    socket.exists().then(|| format!("unix:path={}", escape(socket.as_os_str().as_encoded_bytes())))
}

/// Escapes a value for a D-Bus address, where `,`, `;`, `=` and the like mean something.
fn escape(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for &b in bytes {
        if b.is_ascii_alphanumeric() || b"-_/.\\*".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02x}"));
        }
    }
    out
}

/// Where the system bus is: `DBUS_SYSTEM_BUS_ADDRESS` if it's set, or else the standard
/// socket.
pub fn system_address(env: Option<&str>) -> String {
    env.filter(|a| !a.is_empty()).unwrap_or("unix:path=/run/dbus/system_bus_socket").to_string()
}

fn timed_out() -> zbus::Error {
    zbus::Error::InputOutput(Arc::new(std::io::Error::from(std::io::ErrorKind::TimedOut)))
}

fn spawn_error(e: std::io::Error) -> zbus::Error {
    zbus::Error::InputOutput(Arc::new(e))
}

/// Runs `f` to completion, or gives up after `limit`. A thread of its own wakes the wait
/// when time's up, and leaves as soon as `f` finishes.
fn block_on_for<T>(f: impl Future<Output = zbus::Result<T>>, limit: Duration) -> zbus::Result<T> {
    let fired = Arc::new(AtomicBool::new(false));
    let waker: Arc<Mutex<Option<Waker>>> = Arc::default();
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let (fired2, waker2) = (fired.clone(), waker.clone());
    thread::Builder::new()
        .name("gannet-bus-timer".into())
        .spawn(move || {
            if done_rx.recv_timeout(limit) == Err(mpsc::RecvTimeoutError::Timeout) {
                fired2.store(true, Ordering::SeqCst);
                if let Some(w) = waker2.lock().unwrap().take() {
                    w.wake();
                }
            }
        })
        .map_err(spawn_error)?;
    let mut f = pin!(f);
    let out = block_on(poll_fn(|cx| {
        if let Poll::Ready(r) = f.as_mut().poll(cx) {
            return Poll::Ready(r);
        }
        // Stored before the check, so a timer firing in between still wakes this.
        *waker.lock().unwrap() = Some(cx.waker().clone());
        if fired.load(Ordering::SeqCst) { Poll::Ready(Err(timed_out())) } else { Poll::Pending }
    }));
    drop(done_tx);
    out
}

/// A connection to one bus. Clones share the connection.
#[derive(Clone)]
pub struct Bus {
    conn: Connection,
    limit: Duration,
}

impl Bus {
    /// Connects to the bus at a D-Bus address, like `unix:path=/run/user/1000/bus`.
    pub fn connect(address: &str) -> zbus::Result<Self> {
        Self::connect_with_limit(address, TIME_LIMIT)
    }

    /// As `connect`, giving up on connecting, and later on each method call, after `limit`.
    pub fn connect_with_limit(address: &str, limit: Duration) -> zbus::Result<Self> {
        let conn = block_on_for(zbus::conn::Builder::address(address).method_timeout(limit).build(), limit)?;
        Ok(Self { conn, limit })
    }

    /// The zbus connection, for anything this module doesn't wrap.
    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    /// Asks for a well-known name. True if this connection has it now. Another program
    /// that already owns it keeps it: the request neither waits in line nor replaces it.
    /// Serve the objects first: a program can call them as soon as the name is ours.
    pub fn own_name(&self, name: &str) -> zbus::Result<bool> {
        match block_on_for(self.conn.request_name_with_flags(name, RequestNameFlags::DO_NOT_QUEUE), self.limit) {
            Ok(reply) => Ok(matches!(reply, RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner)),
            // zbus reports a name someone else has as an error, but it's an ordinary answer.
            Err(zbus::Error::NameTaken) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Serves `iface` at `path`. Its methods run on zbus's helper thread. False if an
    /// interface of that name is already served there.
    pub fn serve<I: Interface>(&self, path: &str, iface: I) -> zbus::Result<bool> {
        Ok(block_on_for(self.conn.object_server().at(path, iface), self.limit)?.is_none())
    }

    /// Calls a method on another program and waits for its reply, up to the connection's
    /// time limit.
    pub fn call<B, R>(&self, destination: &str, path: &str, interface: &str, method: &str, body: &B) -> zbus::Result<R>
    where
        B: serde::Serialize + DynamicType,
        R: for<'d> DynamicDeserialize<'d>,
    {
        let reply = block_on(self.conn.call_method(Some(destination), path, Some(interface), method, body))?;
        reply.body().deserialize()
    }

    /// Calls `on` with each message matching `rule`, such as another program's signals, on
    /// a thread of its own. An error reading one is passed on too. It stops when `on`
    /// returns false, the connection closes, or the returned `Following` is dropped. The
    /// match is in place before this returns, so nothing sent after it is missed.
    ///
    /// The thread keeps the connection open while it follows, so drop the `Following` to
    /// let a connection go. A panic in `on` ends following, and so does nothing else: keep
    /// `on` from panicking. zbus queues up to 64 messages for it, and drops the oldest
    /// past that, so `on` should hand work on rather than do it.
    pub fn follow(&self, rule: MatchRule<'static>, mut on: impl FnMut(zbus::Result<Message>) -> bool + Send + 'static) -> zbus::Result<Following> {
        let mut stream = block_on(MessageStream::for_match_rule(rule, &self.conn, None))?;
        let stop: Arc<StopFlag> = Arc::default();
        let stop2 = stop.clone();
        thread::Builder::new()
            .name("gannet-bus-follow".into())
            .spawn(move || {
                loop {
                    let next = block_on(poll_fn(|cx| {
                        *stop2.waker.lock().unwrap() = Some(cx.waker().clone());
                        if stop2.stopped.load(Ordering::SeqCst) {
                            return Poll::Ready(None);
                        }
                        Pin::new(&mut stream).poll_next(cx)
                    }));
                    let Some(m) = next else { return };
                    if !on(m) {
                        return;
                    }
                }
            })
            .map_err(spawn_error)?;
        Ok(Following { stop })
    }
}

#[derive(Default)]
struct StopFlag {
    stopped: AtomicBool,
    waker: Mutex<Option<Waker>>,
}

/// Following messages from `Bus::follow`. Dropping it stops at once, without waiting for
/// another message.
pub struct Following {
    stop: Arc<StopFlag>,
}

impl Drop for Following {
    fn drop(&mut self) {
        self.stop.stopped.store(true, Ordering::SeqCst);
        if let Some(w) = self.stop.waker.lock().unwrap().take() {
            w.wake();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::Scratch;
    use std::io::{BufRead, BufReader};
    use std::process::{Child, Command, Stdio};
    use std::sync::mpsc::{Sender, channel};
    use std::sync::Mutex;
    use std::time::Duration;

    /// A bus of our own, so tests never touch the real session bus.
    struct PrivateBus {
        daemon: Child,
        address: String,
        _dir: Scratch,
    }

    impl PrivateBus {
        fn start() -> Self {
            let dir = Scratch::new();
            // A config of our own, so nothing from the system's bus setup comes along.
            let config = dir.path().join("bus.conf");
            std::fs::write(&config, format!(
                r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:path={}</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
"#,
                dir.path().join("bus").display()
            ))
            .unwrap();
            let daemon = Command::new("dbus-daemon")
                .args(["--nofork", "--print-address=1", &format!("--config-file={}", config.display())])
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .expect("dbus-daemon must be installed to run the D-Bus tests");
            // Owned before anything can fail, so the daemon is always stopped.
            let mut bus = Self { daemon, address: String::new(), _dir: dir };
            let mut line = String::new();
            BufReader::new(bus.daemon.stdout.take().unwrap()).read_line(&mut line).unwrap();
            assert!(!line.trim().is_empty(), "dbus-daemon printed no address");
            bus.address = line.trim().to_string();
            bus
        }
    }

    impl Drop for PrivateBus {
        fn drop(&mut self) {
            let _ = self.daemon.kill();
            let _ = self.daemon.wait();
        }
    }

    /// Answers calls itself, and tells the "UI" about each one, as served objects in the
    /// app will tell its message hub.
    struct Echo {
        told: Mutex<Sender<String>>,
    }

    #[zbus::interface(name = "com.aliasproject.Gannet.Test1")]
    impl Echo {
        fn echo(&self, text: String) -> String {
            let _ = self.told.lock().unwrap().send(text.clone());
            format!("echo: {text}")
        }

        /// Never answers, like a program that's stuck.
        async fn stall(&self) {
            std::future::pending::<()>().await
        }
    }

    fn echo() -> (Echo, std::sync::mpsc::Receiver<String>) {
        let (tx, rx) = channel();
        (Echo { told: Mutex::new(tx) }, rx)
    }

    const NAME: &str = "com.aliasproject.Gannet.Test";
    const PATH: &str = "/com/aliasproject/Gannet/Test";
    const IFACE: &str = "com.aliasproject.Gannet.Test1";

    #[test]
    fn owns_a_name_and_answers_calls_on_its_own_thread() {
        let bus = PrivateBus::start();
        let gannet = Bus::connect(&bus.address).unwrap();
        let (e, rx) = echo();
        assert!(gannet.serve(PATH, e).unwrap());
        assert!(gannet.own_name(NAME).unwrap());
        // Another program calls it. Nothing on this thread has to run for the answer.
        let other = Bus::connect(&bus.address).unwrap();
        let reply: String = other.call(NAME, PATH, IFACE, "Echo", &("hello",)).unwrap();
        assert_eq!(reply, "echo: hello");
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), "hello");
    }

    #[test]
    fn a_name_someone_else_owns_stays_theirs() {
        let bus = PrivateBus::start();
        let first = Bus::connect(&bus.address).unwrap();
        let second = Bus::connect(&bus.address).unwrap();
        let (e, _rx) = echo();
        first.serve(PATH, e).unwrap();
        assert!(first.own_name(NAME).unwrap());
        assert!(first.own_name(NAME).unwrap(), "asking again for a name it has is fine");
        assert!(!second.own_name(NAME).unwrap());
        // The first keeps answering under it.
        let reply: String = second.call(NAME, PATH, IFACE, "Echo", &("x",)).unwrap();
        assert_eq!(reply, "echo: x");
    }

    #[test]
    fn serving_the_same_interface_twice_at_a_path_is_refused() {
        let bus = PrivateBus::start();
        let gannet = Bus::connect(&bus.address).unwrap();
        assert!(gannet.serve(PATH, echo().0).unwrap());
        assert!(!gannet.serve(PATH, echo().0).unwrap());
    }

    #[test]
    fn follows_another_programs_signals_until_told_to_stop() {
        let bus = PrivateBus::start();
        let gannet = Bus::connect(&bus.address).unwrap();
        let other = Bus::connect(&bus.address).unwrap();
        let (tx, rx) = channel();
        let rule = MatchRule::builder().msg_type(zbus::message::Type::Signal).interface(IFACE).member("Changed").build().unwrap();
        let _following = gannet
            .follow(rule, move |m| {
                let n: u32 = m.unwrap().body().deserialize().unwrap();
                let _ = tx.send(n);
                n < 2
            })
            .unwrap();
        for n in 1..=3u32 {
            zbus::block_on(other.connection().emit_signal(None::<&str>, PATH, IFACE, "Changed", &(n,))).unwrap();
        }
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), 1);
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), 2);
        assert!(rx.recv_timeout(Duration::from_millis(300)).is_err(), "it stopped after saying so");
    }

    #[test]
    fn a_call_to_nobody_fails_rather_than_hanging() {
        let bus = PrivateBus::start();
        let gannet = Bus::connect(&bus.address).unwrap();
        let r: zbus::Result<String> = gannet.call("com.example.Nobody", "/", "com.example.Nobody", "Hello", &());
        assert!(r.is_err());
    }

    #[test]
    fn no_bus_is_an_error_not_a_hang() {
        let dir = Scratch::new();
        assert!(Bus::connect(&format!("unix:path={}", dir.path().join("none").display())).is_err());
    }

    #[test]
    fn addresses_come_from_the_environment_or_the_usual_places() {
        let dir = Scratch::new();
        assert_eq!(session_address(Some("unix:path=/x"), Some(dir.path())), Some("unix:path=/x".into()));
        assert_eq!(session_address(None, Some(dir.path())), None, "no socket there");
        std::fs::write(dir.path().join("bus"), b"").unwrap();
        assert_eq!(session_address(Some(""), Some(dir.path())), Some(format!("unix:path={}", dir.path().join("bus").display())));
        assert_eq!(session_address(None, None), None);
        assert_eq!(system_address(None), "unix:path=/run/dbus/system_bus_socket");
        assert_eq!(system_address(Some("unix:path=/y")), "unix:path=/y");
        let odd = dir.path().join("a,b;c=d e");
        std::fs::create_dir(&odd).unwrap();
        std::fs::write(odd.join("bus"), b"").unwrap();
        let a = session_address(None, Some(&odd)).unwrap();
        assert!(a.ends_with("/a%2cb%3bc%3dd%20e/bus"), "{a}");
        assert!(a.parse::<zbus::Address>().is_ok(), "{a}");
    }

    #[test]
    fn clones_share_one_connection() {
        let bus = PrivateBus::start();
        let a = Bus::connect(&bus.address).unwrap();
        let b = a.clone();
        assert_eq!(a.connection().unique_name(), b.connection().unique_name());
        // Worker threads share one.
        fn shareable<T: Send + Sync>() {}
        shareable::<Bus>();
    }

    #[test]
    fn another_process_gets_an_answer_while_nothing_here_drives_the_bus() {
        let bus = PrivateBus::start();
        let gannet = Bus::connect(&bus.address).unwrap();
        let (e, rx) = echo();
        gannet.serve(PATH, e).unwrap();
        gannet.own_name(NAME).unwrap();
        // busctl is a separate program, and this thread only waits for it to exit.
        let out = Command::new("busctl").args([&format!("--address={}", bus.address), "call", NAME, PATH, IFACE, "Echo", "s", "hello"]).output().expect("busctl must be installed to run the D-Bus tests");
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), r#"s "echo: hello""#);
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), "hello");
    }

    #[test]
    fn a_program_that_never_answers_runs_out_of_time() {
        let bus = PrivateBus::start();
        let gannet = Bus::connect(&bus.address).unwrap();
        gannet.serve(PATH, echo().0).unwrap();
        gannet.own_name(NAME).unwrap();
        let caller = Bus::connect_with_limit(&bus.address, Duration::from_millis(300)).unwrap();
        let started = std::time::Instant::now();
        let r: zbus::Result<()> = caller.call(NAME, PATH, IFACE, "Stall", &());
        assert!(r.is_err());
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_bus_that_never_finishes_connecting_runs_out_of_time() {
        let dir = Scratch::new();
        let path = dir.path().join("silent");
        // Accepts connections, then says nothing.
        let _listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let started = std::time::Instant::now();
        assert!(Bus::connect_with_limit(&format!("unix:path={}", path.display()), Duration::from_millis(300)).is_err());
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn dropping_a_follow_stops_it_without_waiting_for_a_message() {
        let bus = PrivateBus::start();
        let gannet = Bus::connect(&bus.address).unwrap();
        let (tx, rx) = channel::<()>();
        let rule = MatchRule::builder().msg_type(zbus::message::Type::Signal).interface(IFACE).build().unwrap();
        let following = gannet
            .follow(rule, move |_| {
                let _ = &tx;
                true
            })
            .unwrap();
        drop(following);
        // The thread let go of `on`, and with it the only sender.
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)), Err(std::sync::mpsc::RecvTimeoutError::Disconnected));
    }
}
