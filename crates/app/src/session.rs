//! Watches the session bus for what the window offers depending on other programs: whether
//! the desktop portal can set the background, and whether a screen reader is running. One
//! thread and one connection do both, and otherwise wait.

use std::sync::Arc;
use std::sync::mpsc::{Sender, channel};
use std::time::Duration;

use gannet_core::bus::{Bus, zbus};
use gannet_core::wallpaper::{self, PORTAL};

use crate::a11y::Shared;
use crate::a11y_atspi;
use crate::app::{Hub, Msg};

const CONNECT_LIMIT: Duration = Duration::from_secs(5);
/// at-spi-bus-launcher, which starts the accessibility bus and says whether to use it.
const A11Y_BUS: &str = "org.a11y.Bus";
const A11Y_PATH: &str = "/org/a11y/bus";
const PROPS: &str = "org.freedesktop.DBus.Properties";

enum Seen {
    /// The portal started (true) or went.
    Portal(bool),
    /// at-spi-bus-launcher's IsEnabled may have changed: look again.
    A11y,
    /// at-spi-bus-launcher started or went. A server on its old bus has lost it.
    Launcher,
    /// Server number such-and-such stopped on its own: its bus went away, or it couldn't start.
    Stopped(u64),
    /// The session bus went away.
    Gone,
}

/// Says the session bus went away: the thread following it drops this as it ends.
struct Gone(Sender<Seen>);

impl Drop for Gone {
    fn drop(&mut self) {
        let _ = self.0.send(Seen::Gone);
    }
}

/// Starts watching the session bus at `address`. Sends `Msg::CanSetBackground` whenever
/// that changes, and serves the window to screen readers while one is running, through
/// `a11y`. `a11y_address` is `AT_SPI_BUS_ADDRESS`, which, when set, says where the
/// accessibility bus is instead of asking. Without a session bus it gives up quietly.
pub fn watch(hub: Hub, address: String, a11y: Arc<Shared>, a11y_address: Option<String>) {
    let _ = std::thread::Builder::new().name("gannet-session".into()).spawn(move || {
        let Ok(bus) = Bus::connect_with_limit(&address, CONNECT_LIMIT) else { return };
        let (tx, rx) = channel();
        let owner = |name: &'static str| zbus::MatchRule::builder().msg_type(zbus::message::Type::Signal).sender("org.freedesktop.DBus").member("NameOwnerChanged").arg(0, name).build();
        let enabled = zbus::MatchRule::builder().msg_type(zbus::message::Type::Signal).sender(A11Y_BUS).interface(PROPS).member("PropertiesChanged").path(A11Y_PATH).build();
        let (Ok(portal), Ok(launcher), Ok(enabled)) = (owner(PORTAL), owner(A11Y_BUS), enabled) else { return };
        let (gone, servers) = (Gone(tx.clone()), tx);
        let Ok(_following) = bus.follow_all(vec![portal, launcher, enabled], None, move |m| {
            let Ok(m) = m else { return true };
            let seen = if m.header().member().is_some_and(|n| n == "NameOwnerChanged") {
                match m.body().deserialize::<(String, String, String)>() {
                    Ok((name, _, new)) if name == PORTAL => Seen::Portal(!new.is_empty()),
                    Ok(_) => Seen::Launcher,
                    Err(_) => return true,
                }
            } else {
                Seen::A11y
            };
            gone.0.send(seen).is_ok()
        }) else {
            return;
        };
        let mut sent = None;
        // Asking starts the portal if it isn't running, which it soon would be anyway: desktop
        // apps ask it for their settings as they start.
        let mut portal_there = true;
        // The accessibility bus being served, and the server's number, if one is.
        let mut serving: Option<(String, u64)> = None;
        let mut check_a11y = true;
        let mut check_portal = true;
        let mut restart = false;
        loop {
            if check_portal {
                let can = portal_there && wallpaper::offered(&bus);
                if sent != Some(can) {
                    sent = Some(can);
                    hub.send(Msg::CanSetBackground(can));
                }
            }
            if check_a11y {
                let want = if is_enabled(&bus) { a11y_bus(&bus, a11y_address.as_deref()) } else { None };
                if want.as_ref() != serving.as_ref().map(|(at, _)| at) || restart {
                    a11y.disconnect();
                    serving = want.map(|at| {
                        let servers = servers.clone();
                        let epoch = a11y_atspi::start(a11y.clone(), hub.clone(), at.clone(), move |epoch| {
                            let _ = servers.send(Seen::Stopped(epoch));
                        });
                        (at, epoch)
                    });
                }
            }
            (check_portal, check_a11y, restart) = (false, false, false);
            match rx.recv() {
                Ok(Seen::Portal(there)) => (portal_there, check_portal) = (there, true),
                Ok(Seen::A11y) => check_a11y = true,
                Ok(Seen::Launcher) => (check_a11y, restart) = (true, true),
                // Not started again straight away, which could go round and round while the
                // bus is unusable: the next change says when to try.
                Ok(Seen::Stopped(epoch)) => {
                    if serving.as_ref().is_some_and(|(_, e)| *e == epoch) {
                        serving = None;
                    }
                }
                Ok(Seen::Gone) | Err(_) => break,
            }
        }
        a11y.disconnect();
    });
}

/// Whether assistive technologies are on: at-spi-bus-launcher's IsEnabled, or its
/// ScreenReaderEnabled, which Orca sets when it starts. Only asked while it's running: asking
/// would otherwise start it, and it says when it starts.
fn is_enabled(bus: &Bus) -> bool {
    let running: zbus::Result<bool> = bus.call("org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus", "NameHasOwner", &(A11Y_BUS,));
    if !running.unwrap_or(false) {
        return false;
    }
    let on = |name: &str| {
        let v: zbus::Result<zbus::OwnedVariant> = bus.call(A11Y_BUS, A11Y_PATH, PROPS, "Get", &("org.a11y.Status", name));
        v.ok().and_then(|v| bool::try_from(&*v).ok()).unwrap_or(false)
    };
    on("IsEnabled") || on("ScreenReaderEnabled")
}

/// Where the accessibility bus is.
fn a11y_bus(bus: &Bus, env: Option<&str>) -> Option<String> {
    if let Some(at) = env.filter(|a| !a.is_empty()) {
        return Some(at.to_string());
    }
    bus.call(A11Y_BUS, A11Y_PATH, A11Y_BUS, "GetAddress", &()).ok().filter(|a: &String| !a.is_empty())
}
