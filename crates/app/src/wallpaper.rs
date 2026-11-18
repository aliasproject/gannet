//! "Set as background", through the desktop portal (see `gannet_core::wallpaper`). The session
//! watcher (`session`) keeps the window told whether the portal can do it. Setting gets a thread
//! of its own, as the portal may first ask the person.

use std::path::PathBuf;
use std::time::Duration;

use gannet_core::bus::Bus;
use gannet_core::wallpaper::{self, Outcome};

use crate::app::{Hub, Msg};

const CONNECT_LIMIT: Duration = Duration::from_secs(5);
/// Long enough for someone to answer the portal's question, if it asks one.
const ANSWER_WAIT: Duration = Duration::from_secs(600);

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
