//! The sidebar's drives, from udisks2 (see `gannet_core::disks`). One thread follows udisks2's
//! signals and sends the window the volumes again after each burst of changes, so nothing runs
//! while nothing changes. Mounting, unlocking, unmounting and ejecting each get a thread of their
//! own, as polkit may keep one waiting on a person.

use std::path::PathBuf;
use std::sync::mpsc::channel;
use std::time::Duration;

use gannet_core::bus::{Bus, zbus};
use gannet_core::disks::{self, DiskError, Udisks, Volume};

use crate::app::{Hub, Msg};

/// A change usually comes as several signals at once, and one look at the end covers them.
const SETTLE: Duration = Duration::from_millis(100);
const CONNECT_LIMIT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, PartialEq)]
pub enum Doing {
    Mount,
    Unlock(String),
    Unmount,
    Eject,
}

impl Doing {
    /// For "Couldn't … “PHOTOS”".
    pub fn verb(&self) -> &'static str {
        match self {
            Doing::Mount => "open",
            Doing::Unlock(_) => "unlock",
            Doing::Unmount => "unmount",
            Doing::Eject => "eject",
        }
    }
}

/// Follows udisks2 on the system bus at `address`, sending `Msg::Volumes` with what's there now,
/// and again whenever it changes. Without udisks2, or a system bus, it gives up quietly, and the
/// sidebar keeps the disks it found mounted.
pub fn watch(hub: Hub, address: String) {
    let _ = std::thread::Builder::new().name("gannet-drives".into()).spawn(move || {
        let Ok(bus) = Bus::connect_with_limit(&address, CONNECT_LIMIT) else { return };
        let (tx, rx) = channel();
        let Ok(rule) = zbus::MatchRule::builder().msg_type(zbus::message::Type::Signal).sender(disks::SERVICE).build() else { return };
        let Ok(_following) = bus.follow(rule, move |_| tx.send(()).is_ok()) else { return };
        let udisks = Udisks::new(bus);
        loop {
            if let Ok(snapshot) = udisks.snapshot() {
                hub.send(Msg::Volumes(snapshot.volumes()));
            }
            if rx.recv().is_err() {
                return;
            }
            while rx.recv_timeout(SETTLE).is_ok() {}
        }
    });
}

/// Does `doing` to `volume`, then sends `Msg::DiskDone`, with where it's mounted for a mount or
/// an unlock.
pub fn act(hub: Hub, address: String, volume: Volume, doing: Doing) {
    let _ = std::thread::Builder::new().name("gannet-drive".into()).spawn(move || {
        let result = Udisks::connect(&address).and_then(|udisks| -> Result<Option<PathBuf>, DiskError> {
            match &doing {
                Doing::Mount => udisks.mount(&volume).map(Some),
                Doing::Unlock(passphrase) => udisks.unlock(&volume, passphrase).map(Some),
                Doing::Unmount => udisks.unmount(&volume).map(|()| None),
                Doing::Eject => udisks.eject(&volume).map(|()| None),
            }
        });
        // The passphrase isn't kept: only what was done goes back.
        let doing = if let Doing::Unlock(_) = doing { Doing::Unlock(String::new()) } else { doing };
        hub.send(Msg::DiskDone { volume, doing, result });
    });
}
