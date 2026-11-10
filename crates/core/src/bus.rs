//! D-Bus, through zbus.
//!
//! zbus is async, and Gannet has no async runtime. Connections run on zbus's own helper
//! thread, which also answers calls to objects served here, so nothing in this module
//! needs Gannet's main loop. Every method here blocks the calling thread until the bus
//! answers, though, and a bus can be slow or gone: call them from a worker thread, never
//! the UI thread. Served objects reach the UI by sending to the app's message hub.
//!
//! Like the rest of this crate, nothing here reads the environment. The app works out
//! the addresses with `session_address` and `system_address` and passes them in.

use std::path::Path;
use std::pin::Pin;
use std::thread;

use futures_core::Stream;
use zbus::fdo::{RequestNameFlags, RequestNameReply};
use zbus::object_server::Interface;
use zbus::wire::{DynamicDeserialize, DynamicType};
use zbus::{Connection, MatchRule, Message, MessageStream, block_on};

pub use zbus;

/// Where the session bus is: `DBUS_SESSION_BUS_ADDRESS` if it's set, or else the
/// `bus` socket in `XDG_RUNTIME_DIR`, which is where systemd's user bus lives.
pub fn session_address(env: Option<&str>, runtime_dir: Option<&Path>) -> Option<String> {
    if let Some(a) = env.filter(|a| !a.is_empty()) {
        return Some(a.to_string());
    }
    let socket = runtime_dir?.join("bus");
    socket.exists().then(|| format!("unix:path={}", socket.display()))
}

/// Where the system bus is: `DBUS_SYSTEM_BUS_ADDRESS` if it's set, or else the standard
/// socket.
pub fn system_address(env: Option<&str>) -> String {
    env.filter(|a| !a.is_empty()).unwrap_or("unix:path=/run/dbus/system_bus_socket").to_string()
}

/// A connection to one bus. Clones share the connection.
#[derive(Clone)]
pub struct Bus {
    conn: Connection,
}

impl Bus {
    /// Connects to the bus at a D-Bus address, like `unix:path=/run/user/1000/bus`.
    pub fn connect(address: &str) -> zbus::Result<Self> {
        let conn = block_on(zbus::conn::Builder::address(address).build())?;
        Ok(Self { conn })
    }

    /// The zbus connection, for anything this module doesn't wrap.
    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    /// Asks for a well-known name. True if this connection has it now. Another program
    /// that already owns it keeps it: the request neither waits in line nor replaces it.
    pub fn own_name(&self, name: &str) -> zbus::Result<bool> {
        match block_on(self.conn.request_name_with_flags(name, RequestNameFlags::DO_NOT_QUEUE)) {
            Ok(reply) => Ok(matches!(reply, RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner)),
            // zbus reports a name someone else has as an error, but it's an ordinary answer.
            Err(zbus::Error::NameTaken) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Serves `iface` at `path`. Its methods run on zbus's helper thread. False if an
    /// interface of that name is already served there.
    pub fn serve<I: Interface>(&self, path: &str, iface: I) -> zbus::Result<bool> {
        Ok(block_on(self.conn.object_server().at(path, iface))?.is_none())
    }

    /// Calls a method on another program and waits for its reply.
    pub fn call<B, R>(&self, destination: &str, path: &str, interface: &str, method: &str, body: &B) -> zbus::Result<R>
    where
        B: serde::Serialize + DynamicType,
        R: for<'d> DynamicDeserialize<'d>,
    {
        let reply = block_on(self.conn.call_method(Some(destination), path, Some(interface), method, body))?;
        reply.body().deserialize()
    }

    /// Calls `on` with each message matching `rule`, such as another program's signals, on
    /// a thread of its own, until `on` returns false or the connection closes. The match is
    /// in place before this returns, so nothing sent after it is missed.
    pub fn follow(&self, rule: MatchRule<'static>, mut on: impl FnMut(Message) -> bool + Send + 'static) -> zbus::Result<()> {
        let mut stream = block_on(MessageStream::for_match_rule(rule, &self.conn, None))?;
        thread::Builder::new()
            .name("gannet-bus-follow".into())
            .spawn(move || {
                while let Some(Ok(m)) = block_on(std::future::poll_fn(|cx| Pin::new(&mut stream).poll_next(cx))) {
                    if !on(m) {
                        break;
                    }
                }
            })
            .map_err(|e| zbus::Error::InputOutput(std::sync::Arc::new(e)))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::Scratch;
    use std::io::{BufRead, BufReader};
    use std::process::{Child, Command, Stdio};
    use std::sync::mpsc::{Sender, channel};
    use std::sync::{Arc, Mutex};
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
            let listen = format!("unix:path={}", dir.path().join("bus").display());
            let mut daemon = Command::new("dbus-daemon")
                .args(["--session", "--nofork", "--print-address=1", &format!("--address={listen}")])
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .expect("dbus-daemon must be installed to run the D-Bus tests");
            let mut line = String::new();
            BufReader::new(daemon.stdout.take().unwrap()).read_line(&mut line).unwrap();
            assert!(!line.trim().is_empty(), "dbus-daemon printed no address");
            Self { daemon, address: line.trim().to_string(), _dir: dir }
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
    }

    const NAME: &str = "com.aliasproject.Gannet.Test";
    const PATH: &str = "/com/aliasproject/Gannet/Test";
    const IFACE: &str = "com.aliasproject.Gannet.Test1";

    #[test]
    fn owns_a_name_and_answers_calls_on_its_own_thread() {
        let bus = PrivateBus::start();
        let gannet = Bus::connect(&bus.address).unwrap();
        let (tx, rx) = channel();
        assert!(gannet.serve(PATH, Echo { told: Mutex::new(tx) }).unwrap());
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
        assert!(first.own_name(NAME).unwrap());
        assert!(first.own_name(NAME).unwrap(), "asking again for a name it has is fine");
        assert!(!second.own_name(NAME).unwrap());
        // The first keeps answering under it.
        let (tx, _rx) = channel();
        first.serve(PATH, Echo { told: Mutex::new(tx) }).unwrap();
        let reply: String = second.call(NAME, PATH, IFACE, "Echo", &("x",)).unwrap();
        assert_eq!(reply, "echo: x");
    }

    #[test]
    fn serving_the_same_interface_twice_at_a_path_is_refused() {
        let bus = PrivateBus::start();
        let gannet = Bus::connect(&bus.address).unwrap();
        let (tx, _rx) = channel();
        assert!(gannet.serve(PATH, Echo { told: Mutex::new(tx.clone()) }).unwrap());
        assert!(!gannet.serve(PATH, Echo { told: Mutex::new(tx) }).unwrap());
    }

    #[test]
    fn follows_another_programs_signals_until_told_to_stop() {
        let bus = PrivateBus::start();
        let gannet = Bus::connect(&bus.address).unwrap();
        let other = Bus::connect(&bus.address).unwrap();
        let (tx, rx) = channel();
        let rule = MatchRule::builder().msg_type(zbus::message::Type::Signal).interface(IFACE).member("Changed").build().unwrap();
        gannet
            .follow(rule, move |m| {
                let n: u32 = m.body().deserialize().unwrap();
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
    }

    #[test]
    fn clones_share_one_connection() {
        let bus = PrivateBus::start();
        let a = Bus::connect(&bus.address).unwrap();
        let b = a.clone();
        assert_eq!(a.connection().unique_name(), b.connection().unique_name());
        let _ = Arc::new(b);
    }
}
