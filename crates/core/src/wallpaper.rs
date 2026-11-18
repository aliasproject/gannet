//! Setting a picture as the desktop background, through the desktop portal's Wallpaper
//! interface. The desktop sets it its own way, so nothing here knows about any one desktop.
//! The portal has the interface only where one of its backends can set a background (GNOME's,
//! KDE's and aliasOS's can), so `offered` says whether to offer it at all.
//!
//! Like `bus`, everything here waits on the bus: call it from a worker thread.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::time::Duration;

use rustix::fs::{Mode, OFlags};
use zbus::{Fd, MatchRule, OwnedObjectPath, OwnedVariant, Variant};

use crate::bus::{Bus, Following, zbus};

/// The desktop portal's bus name.
pub const PORTAL: &str = "org.freedesktop.portal.Desktop";
const PATH: &str = "/org/freedesktop/portal/desktop";
const WALLPAPER: &str = "org.freedesktop.portal.Wallpaper";

/// Whether the portal can set the background. Asking starts the portal if it isn't running.
pub fn offered(bus: &Bus) -> bool {
    bus.call::<_, OwnedVariant>(PORTAL, PATH, "org.freedesktop.DBus.Properties", "Get", &(WALLPAPER, "version")).is_ok()
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Set,
    /// The person said no, in a dialog of the portal's.
    Cancelled,
    /// It couldn't be set, or the person has told the portal never to let Gannet set it:
    /// the portal gives the same answer for both.
    NotSet,
    /// Nothing came back within the wait, as when a permission dialog is left open.
    NoAnswer,
}

/// Asks the portal to make `file` the background, then waits up to `wait` for its answer.
/// The portal may first ask the person, once, whether Gannet may set it.
pub fn set(bus: &Bus, file: &Path, wait: Duration) -> Outcome {
    // O_PATH: the portal only needs to know which file it is, and a FIFO named like a
    // picture can't hold this up.
    let Ok(fd) = rustix::fs::open(file, OFlags::PATH | OFlags::CLOEXEC, Mode::empty()) else { return Outcome::NotSet };
    let Some(sender) = bus.connection().unique_name() else { return Outcome::NotSet };
    let token = next_token();
    let (tx, rx) = channel();
    // The answer comes as a signal from an object the portal makes at a path it builds from
    // this connection's name and the token, so it's followed before asking, and can't be missed.
    let expected = request_path(sender.as_str(), &token);
    let Ok(_following) = follow_response(bus, &expected, tx.clone()) else { return Outcome::NotSet };
    let options: HashMap<&str, Variant> = HashMap::from([("handle_token", Variant::from(token.as_str())), ("show-preview", Variant::from(false)), ("set-on", Variant::from("background"))]);
    let Ok(handle) = bus.call::<_, OwnedObjectPath>(PORTAL, PATH, WALLPAPER, "SetWallpaperFile", &("", Fd::from(&fd), options)) else { return Outcome::NotSet };
    // A portal older than handle tokens picks its own path. Its answer could come before
    // this follows it, and then it's waited for in vain.
    let _also = (handle.as_str() != expected).then(|| follow_response(bus, handle.as_str(), tx));
    match rx.recv_timeout(wait) {
        Ok(0) => Outcome::Set,
        Ok(1) => Outcome::Cancelled,
        Ok(_) => Outcome::NotSet,
        Err(_) => Outcome::NoAnswer,
    }
}

/// Sends the response code of the portal request at `path`, once it comes.
fn follow_response(bus: &Bus, path: &str, tx: Sender<u32>) -> zbus::Result<Following> {
    let rule = MatchRule::builder().msg_type(zbus::message::Type::Signal).interface("org.freedesktop.portal.Request").member("Response").path(path.to_string()).build()?;
    bus.follow(rule, move |m| {
        let Ok((code, _)) = m.and_then(|m| m.body().deserialize::<(u32, HashMap<String, OwnedVariant>)>()) else { return true };
        let _ = tx.send(code);
        false
    })
}

/// Where the portal puts the request: its path, the sender's name without the colon and
/// with `_` for each `.`, then the token.
fn request_path(sender: &str, token: &str) -> String {
    format!("{PATH}/request/{}/{token}", sender.trim_start_matches(':').replace('.', "_"))
}

/// A token no other request from this connection has. The portal only allows letters,
/// digits and `_`.
fn next_token() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!("gannet{}_{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{PrivateBus, Scratch};
    use std::sync::Mutex;
    use zbus::object_server::SignalEmitter;

    /// A request the stand-in portal got: the file, as its descriptor names it, and the options.
    type Told = (String, HashMap<String, OwnedVariant>);

    /// Stands in for the portal: it records each request, and answers with `code` from the
    /// path built from the handle token, or from a path of its own like an old portal.
    struct FakeWallpaper {
        /// `None` never answers.
        code: Option<u32>,
        own_path: bool,
        told: Mutex<Sender<Told>>,
    }

    #[zbus::interface(name = "org.freedesktop.portal.Wallpaper")]
    impl FakeWallpaper {
        async fn set_wallpaper_file(
            &self,
            #[zbus(header)] header: zbus::message::Header<'_>,
            #[zbus(connection)] conn: &zbus::Connection,
            _parent: String,
            fd: zbus::OwnedFd,
            options: HashMap<String, OwnedVariant>,
        ) -> OwnedObjectPath {
            // What the descriptor is open on, as the real portal works it out.
            let named = std::fs::read_link(format!("/proc/self/fd/{}", std::os::fd::AsRawFd::as_raw_fd(&fd))).unwrap();
            let sender = header.sender().unwrap().to_string();
            let token: String = options.get("handle_token").and_then(|v| String::try_from(v.try_clone().unwrap()).ok()).unwrap();
            let path = if self.own_path { format!("{PATH}/request/old/1") } else { request_path(&sender, &token) };
            let _ = self.told.lock().unwrap().send((named.display().to_string(), options));
            let (conn, at) = (conn.clone(), path.clone());
            // The answer comes after the reply, as the real portal's does.
            let Some(code) = self.code else { return OwnedObjectPath::try_from(path).unwrap() };
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(50));
                let emitter = SignalEmitter::new(&conn, at.as_str()).unwrap();
                zbus::block_on(emitter.emit("org.freedesktop.portal.Request", "Response", &(code, HashMap::<String, OwnedVariant>::new()))).unwrap();
            });
            OwnedObjectPath::try_from(path).unwrap()
        }

        #[zbus(property, name = "version")]
        fn version(&self) -> u32 {
            1
        }
    }

    fn portal(bus: &PrivateBus, code: Option<u32>, own_path: bool) -> (Bus, std::sync::mpsc::Receiver<Told>) {
        let (tx, rx) = channel();
        let portal = Bus::connect(&bus.address).unwrap();
        assert!(portal.serve(PATH, FakeWallpaper { code, own_path, told: Mutex::new(tx) }).unwrap());
        assert!(portal.own_name(PORTAL).unwrap());
        (portal, rx)
    }

    #[test]
    fn offered_only_where_the_portal_has_the_wallpaper_interface() {
        let bus = PrivateBus::start();
        let gannet = Bus::connect(&bus.address).unwrap();
        assert!(!offered(&gannet), "no portal at all");
        // Other portals, like Settings, are served at the same path.
        struct Settings;
        #[zbus::interface(name = "org.freedesktop.portal.Settings")]
        impl Settings {}
        let without = Bus::connect(&bus.address).unwrap();
        assert!(without.serve(PATH, Settings).unwrap());
        assert!(without.own_name(PORTAL).unwrap());
        assert!(!offered(&gannet), "a portal without the Wallpaper interface");
        assert!(without.release_name(PORTAL).unwrap());
        let _portal = portal(&bus, Some(0), false);
        assert!(offered(&gannet));
    }

    #[test]
    fn hands_the_portal_the_picture_and_reports_its_answer() {
        let scratch = Scratch::new();
        let picture = scratch.file("sunset at sea.png", b"not really a png");
        for (code, outcome) in [(0, Outcome::Set), (1, Outcome::Cancelled), (2, Outcome::NotSet)] {
            let bus = PrivateBus::start();
            let (_portal, told) = portal(&bus, Some(code), false);
            let gannet = Bus::connect(&bus.address).unwrap();
            assert_eq!(set(&gannet, &picture, Duration::from_secs(5)), outcome);
            let (named, options) = told.recv_timeout(Duration::from_secs(5)).unwrap();
            assert_eq!(named, picture.canonicalize().unwrap().display().to_string());
            assert_eq!(bool::try_from(options["show-preview"].try_clone().unwrap()), Ok(false));
            assert_eq!(String::try_from(options["set-on"].try_clone().unwrap()).as_deref(), Ok("background"));
        }
    }

    #[test]
    fn an_answer_from_a_path_the_portal_chose_still_counts() {
        let scratch = Scratch::new();
        let picture = scratch.file("a.png", b"");
        let bus = PrivateBus::start();
        let (_portal, _told) = portal(&bus, Some(0), true);
        let gannet = Bus::connect(&bus.address).unwrap();
        assert_eq!(set(&gannet, &picture, Duration::from_secs(5)), Outcome::Set);
    }

    #[test]
    fn a_missing_file_or_a_silent_portal_is_reported_not_waited_on_forever() {
        let scratch = Scratch::new();
        let bus = PrivateBus::start();
        let gannet = Bus::connect(&bus.address).unwrap();
        assert_eq!(set(&gannet, &scratch.path().join("gone.png"), Duration::from_secs(5)), Outcome::NotSet);
        // No portal: the call fails at once.
        let picture = scratch.file("a.png", b"");
        assert_eq!(set(&gannet, &picture, Duration::from_secs(5)), Outcome::NotSet);
        let (_portal, _told) = portal(&bus, None, false);
        assert_eq!(set(&gannet, &picture, Duration::from_millis(300)), Outcome::NoAnswer);
    }

    #[test]
    fn request_paths_follow_the_portals_rule() {
        assert_eq!(request_path(":1.42", "gannet7_0"), "/org/freedesktop/portal/desktop/request/1_42/gannet7_0");
        let (a, b) = (next_token(), next_token());
        assert_ne!(a, b);
        assert!(a.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
    }
}
