//! "Set as background", through the desktop portal (see `gannet_core::wallpaper`). One thread
//! keeps the window told whether the portal can do it: the portal's interfaces only change when
//! it starts, so it looks again each time the portal starts, and otherwise waits. Setting gets a
//! thread of its own, as the portal may first ask the person.

use std::path::PathBuf;
use std::sync::mpsc::channel;
use std::time::Duration;

use gannet_core::bus::{Bus, zbus};
use gannet_core::wallpaper::{self, Outcome, PORTAL};

use crate::app::{Hub, Msg};

const CONNECT_LIMIT: Duration = Duration::from_secs(5);
/// Long enough for someone to answer the portal's question, if it asks one.
const ANSWER_WAIT: Duration = Duration::from_secs(600);

/// Sends `Msg::CanSetBackground` with whether the portal on the session bus at `address` can
/// set the background, and again whenever that changes. Without a session bus it gives up
/// quietly, and the menu never offers it.
pub fn watch(hub: Hub, address: String) {
    let _ = std::thread::Builder::new().name("gannet-wallpaper".into()).spawn(move || {
        let Ok(bus) = Bus::connect_with_limit(&address, CONNECT_LIMIT) else { return };
        let (tx, rx) = channel();
        let Ok(rule) = zbus::MatchRule::builder().msg_type(zbus::message::Type::Signal).sender("org.freedesktop.DBus").member("NameOwnerChanged").arg(0, PORTAL).build() else { return };
        // Each change of hands says whether the portal is there now: its new owner, or none.
        let Ok(_following) = bus.follow(rule, move |m| {
            let started = m.ok().and_then(|m| m.body().deserialize::<(String, String, String)>().ok()).map(|(_, _, new)| !new.is_empty());
            tx.send(started).is_ok()
        }) else {
            return;
        };
        let mut sent = None;
        // Asking starts the portal if it isn't running, which it soon would be anyway: desktop
        // apps ask it for their settings as they start.
        let mut there = true;
        loop {
            let can = there && wallpaper::offered(&bus);
            if sent != Some(can) {
                sent = Some(can);
                hub.send(Msg::CanSetBackground(can));
            }
            match rx.recv() {
                Ok(Some(started)) => there = started,
                Ok(None) => {}
                Err(_) => return,
            }
        }
    });
}

/// Makes `path` the background, saying so in the window only if it couldn't be done.
pub fn set(hub: Hub, address: String, path: PathBuf) {
    let _ = std::thread::Builder::new().name("gannet-set-background".into()).spawn(move || {
        let outcome = Bus::connect_with_limit(&address, CONNECT_LIMIT).map_or(Outcome::NotSet, |bus| wallpaper::set(&bus, &path, ANSWER_WAIT));
        if outcome == Outcome::NotSet {
            let name = path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
            hub.send(Msg::Notice(format!("Couldn't set “{name}” as the background")));
        }
    });
}
