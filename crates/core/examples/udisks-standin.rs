//! A stand-in for udisks2, for the checks: fake drives on a private bus, which "mount" onto
//! folders under a scratch root rather than mounting anything. It reads commands on stdin, one a
//! line, and answers each with `ok` or `error ...`:
//!
//!   drive NAME ID         a removable drive, which can be powered off
//!   fs NAME DRIVE LABEL   a filesystem on it, mounting at ROOT/LABEL
//!   luks NAME DRIVE PASSPHRASE LABEL   an encrypted volume holding a filesystem called LABEL
//!   unplug DRIVE          the drive and everything on it go
//!   busy NAME             unmounting NAME says it's busy until `idle NAME`
//!   idle NAME
//!   mount NAME            mounted by someone else
//!
//! Usage: udisks-standin ADDRESS ROOT

use std::collections::{BTreeMap, HashMap};
use std::io::BufRead;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use gannet_core::bus::Bus;
use gannet_core::bus::zbus::{self, Connection, OwnedObjectPath, OwnedVariant, Variant, block_on};

const ROOT: &str = "/org/freedesktop/UDisks2";

#[derive(Clone, Default)]
struct DriveState {
    id: String,
}

#[derive(Clone, Default)]
struct BlockState {
    drive: String,
    label: String,
    usage: String,
    backing: String,
    /// `Some` for a filesystem: where it's mounted, if it is.
    mounted: Option<Option<PathBuf>>,
    /// `Some` for an encrypted volume: its passphrase, the label inside, and its contents' path.
    luks: Option<(String, String, Option<String>)>,
    busy: bool,
}

#[derive(Default)]
struct State {
    root: PathBuf,
    drives: BTreeMap<String, DriveState>,
    blocks: BTreeMap<String, BlockState>,
}

type Shared = Arc<Mutex<State>>;

#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.freedesktop.UDisks2.Error")]
enum UdError {
    #[zbus(error)]
    ZBus(zbus::Error),
    DeviceBusy(String),
    Failed(String),
    AlreadyMounted(String),
    NotMounted(String),
}

fn path(kind: &str, name: &str) -> String {
    format!("{ROOT}/{kind}/{name}")
}

fn object(p: &str) -> OwnedObjectPath {
    OwnedObjectPath::try_from(p.to_string()).unwrap()
}

/// Tells anyone following that `iface` on `at` has changed, as udisks2 does.
async fn changed(conn: &Connection, at: &str, iface: &str) {
    let props: HashMap<&str, Variant> = HashMap::new();
    let _ = conn.emit_signal(None::<()>, at, "org.freedesktop.DBus.Properties", "PropertiesChanged", &(format!("org.freedesktop.UDisks2.{iface}"), props, vec!["MountPoints"])).await;
}

struct DriveIface {
    at: String,
    state: Shared,
}

#[zbus::interface(name = "org.freedesktop.UDisks2.Drive")]
impl DriveIface {
    #[zbus(property)]
    fn id(&self) -> String {
        self.state.lock().unwrap().drives.get(&self.at).map(|d| d.id.clone()).unwrap_or_default()
    }
    #[zbus(property)]
    fn size(&self) -> u64 {
        16_000_000_000
    }
    #[zbus(property)]
    fn removable(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn ejectable(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn can_power_off(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn time_media_detected(&self) -> u64 {
        1_700_000_000_000_000
    }

    fn power_off(&self, _options: HashMap<String, OwnedVariant>, #[zbus(connection)] conn: &Connection) -> Result<(), UdError> {
        let (conn, state, at) = (conn.clone(), self.state.clone(), self.at.clone());
        // Out of the way of this call, which the object being taken away would hold up.
        std::thread::spawn(move || unplug(&conn, &state, &at));
        Ok(())
    }
}

struct BlockIface {
    at: String,
    state: Shared,
}

impl BlockIface {
    /// A block can be read in the moment between leaving the state and leaving the bus, and
    /// then it reads as nothing.
    fn get<T>(&self, f: impl FnOnce(&BlockState) -> T) -> T {
        let gone = BlockState { drive: "/".into(), backing: "/".into(), ..BlockState::default() };
        f(self.state.lock().unwrap().blocks.get(&self.at).unwrap_or(&gone))
    }
}

#[zbus::interface(name = "org.freedesktop.UDisks2.Block")]
impl BlockIface {
    #[zbus(property)]
    fn drive(&self) -> OwnedObjectPath {
        object(&self.get(|b| b.drive.clone()))
    }
    #[zbus(property)]
    fn id_label(&self) -> String {
        self.get(|b| if b.luks.is_some() { String::new() } else { b.label.clone() })
    }
    #[zbus(property)]
    fn id_usage(&self) -> String {
        self.get(|b| b.usage.clone())
    }
    #[zbus(property)]
    fn size(&self) -> u64 {
        8_000_000_000
    }
    #[zbus(property)]
    fn hint_system(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn hint_ignore(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn crypto_backing_device(&self) -> OwnedObjectPath {
        object(&self.get(|b| b.backing.clone()))
    }
}

struct FilesystemIface {
    at: String,
    state: Shared,
}

#[zbus::interface(name = "org.freedesktop.UDisks2.Filesystem")]
impl FilesystemIface {
    #[zbus(property)]
    fn mount_points(&self) -> Vec<Vec<u8>> {
        let state = self.state.lock().unwrap();
        let Some(Some(Some(at))) = state.blocks.get(&self.at).map(|b| b.mounted.clone()) else { return Vec::new() };
        vec![[at.as_os_str().as_encoded_bytes(), b"\0"].concat()]
    }

    async fn mount(&self, _options: HashMap<String, OwnedVariant>, #[zbus(connection)] conn: &Connection) -> Result<String, UdError> {
        let at = {
            let mut state = self.state.lock().unwrap();
            let root = state.root.clone();
            let b = state.blocks.get_mut(&self.at).ok_or_else(|| UdError::Failed("gone".into()))?;
            if let Some(Some(_)) = b.mounted {
                return Err(UdError::AlreadyMounted("already mounted".into()));
            }
            let at = root.join(&b.label);
            std::fs::create_dir_all(&at).map_err(|e| UdError::Failed(e.to_string()))?;
            b.mounted = Some(Some(at.clone()));
            at
        };
        changed(conn, &self.at, "Filesystem").await;
        Ok(at.to_string_lossy().into_owned())
    }

    async fn unmount(&self, _options: HashMap<String, OwnedVariant>, #[zbus(connection)] conn: &Connection) -> Result<(), UdError> {
        {
            let mut state = self.state.lock().unwrap();
            let b = state.blocks.get_mut(&self.at).ok_or_else(|| UdError::Failed("gone".into()))?;
            if b.busy {
                return Err(UdError::DeviceBusy("Error unmounting: target is busy".into()));
            }
            if !matches!(b.mounted, Some(Some(_))) {
                return Err(UdError::NotMounted("not mounted".into()));
            }
            b.mounted = Some(None);
        }
        changed(conn, &self.at, "Filesystem").await;
        Ok(())
    }
}

struct EncryptedIface {
    at: String,
    state: Shared,
}

#[zbus::interface(name = "org.freedesktop.UDisks2.Encrypted")]
impl EncryptedIface {
    #[zbus(property)]
    fn cleartext_device(&self) -> OwnedObjectPath {
        let state = self.state.lock().unwrap();
        object(state.blocks.get(&self.at).and_then(|b| b.luks.as_ref()?.2.clone()).as_deref().unwrap_or("/"))
    }

    async fn unlock(&self, passphrase: String, _options: HashMap<String, OwnedVariant>, #[zbus(connection)] conn: &Connection) -> Result<OwnedObjectPath, UdError> {
        let inside = {
            let mut state = self.state.lock().unwrap();
            let b = state.blocks.get_mut(&self.at).ok_or_else(|| UdError::Failed("gone".into()))?;
            let Some((secret, label, open)) = b.luks.as_mut() else { return Err(UdError::Failed("not encrypted".into())) };
            if open.is_some() {
                return Err(UdError::Failed("already unlocked".into()));
            }
            if passphrase != *secret {
                return Err(UdError::Failed("Error unlocking: Failed to activate device: Incorrect passphrase.".into()));
            }
            let name = format!("dm_{}", self.at.rsplit('/').next().unwrap_or("x"));
            let inside = path("block_devices", &name);
            *open = Some(inside.clone());
            let label = label.clone();
            state.blocks.insert(inside.clone(), BlockState { drive: "/".into(), label, usage: "filesystem".into(), backing: self.at.clone(), mounted: Some(None), ..BlockState::default() });
            inside
        };
        let server = conn.object_server();
        server.at(inside.as_str(), BlockIface { at: inside.clone(), state: self.state.clone() }).await?;
        server.at(inside.as_str(), FilesystemIface { at: inside.clone(), state: self.state.clone() }).await?;
        changed(conn, &self.at, "Encrypted").await;
        Ok(object(&inside))
    }

    async fn lock(&self, _options: HashMap<String, OwnedVariant>, #[zbus(connection)] conn: &Connection) -> Result<(), UdError> {
        let inside = {
            let mut state = self.state.lock().unwrap();
            let b = state.blocks.get_mut(&self.at).ok_or_else(|| UdError::Failed("gone".into()))?;
            let inside = b.luks.as_mut().and_then(|l| l.2.take()).ok_or_else(|| UdError::Failed("not unlocked".into()))?;
            state.blocks.remove(&inside);
            inside
        };
        let server = conn.object_server();
        let _ = server.remove::<FilesystemIface, _>(inside.as_str()).await;
        let _ = server.remove::<BlockIface, _>(inside.as_str()).await;
        changed(conn, &self.at, "Encrypted").await;
        Ok(())
    }
}

/// Serves a block's interfaces, as its state says it has.
fn serve_block(conn: &Connection, state: &Shared, at: &str) {
    let (fs, luks) = {
        let s = state.lock().unwrap();
        let b = &s.blocks[at];
        (b.mounted.is_some(), b.luks.is_some())
    };
    let server = conn.object_server();
    block_on(server.at(at, BlockIface { at: at.into(), state: state.clone() })).unwrap();
    if fs {
        block_on(server.at(at, FilesystemIface { at: at.into(), state: state.clone() })).unwrap();
    }
    if luks {
        block_on(server.at(at, EncryptedIface { at: at.into(), state: state.clone() })).unwrap();
    }
}

fn remove(conn: &Connection, at: &str) {
    let server = conn.object_server();
    let _ = block_on(server.remove::<FilesystemIface, _>(at));
    let _ = block_on(server.remove::<EncryptedIface, _>(at));
    let _ = block_on(server.remove::<BlockIface, _>(at));
    let _ = block_on(server.remove::<DriveIface, _>(at));
}

fn unplug(conn: &Connection, state: &Shared, drive: &str) {
    let gone: Vec<String> = {
        let mut s = state.lock().unwrap();
        s.drives.remove(drive);
        let on: Vec<String> = s.blocks.iter().filter(|(_, b)| b.drive == drive).map(|(p, _)| p.clone()).collect();
        let inside: Vec<String> = s.blocks.iter().filter(|(_, b)| on.contains(&b.backing)).map(|(p, _)| p.clone()).collect();
        let all: Vec<String> = on.into_iter().chain(inside).collect();
        for p in &all {
            s.blocks.remove(p);
        }
        all
    };
    for p in gone.iter().chain([&drive.to_string()]) {
        remove(conn, p);
    }
}

fn command(conn: &Connection, state: &Shared, line: &str) -> Result<(), String> {
    let words: Vec<&str> = line.split_whitespace().collect();
    match words.as_slice() {
        ["drive", name, id] => {
            let at = path("drives", name);
            state.lock().unwrap().drives.insert(at.clone(), DriveState { id: id.to_string() });
            block_on(conn.object_server().at(at.as_str(), DriveIface { at: at.clone(), state: state.clone() })).map_err(|e| e.to_string())?;
        }
        ["fs", name, drive, label] => {
            let at = path("block_devices", name);
            state.lock().unwrap().blocks.insert(at.clone(), BlockState { drive: path("drives", drive), label: label.to_string(), usage: "filesystem".into(), backing: "/".into(), mounted: Some(None), ..BlockState::default() });
            serve_block(conn, state, &at);
        }
        ["luks", name, drive, passphrase, label] => {
            let at = path("block_devices", name);
            state.lock().unwrap().blocks.insert(at.clone(), BlockState { drive: path("drives", drive), label: String::new(), usage: "crypto".into(), backing: "/".into(), luks: Some((passphrase.to_string(), label.to_string(), None)), ..BlockState::default() });
            serve_block(conn, state, &at);
        }
        ["unplug", drive] => unplug(conn, state, &path("drives", drive)),
        ["busy" | "idle", name] => {
            let at = path("block_devices", name);
            state.lock().unwrap().blocks.get_mut(&at).ok_or("no such block")?.busy = words[0] == "busy";
        }
        ["mount", name] => {
            let at = path("block_devices", name);
            {
                let mut s = state.lock().unwrap();
                let root = s.root.clone();
                let b = s.blocks.get_mut(&at).ok_or("no such block")?;
                let to = root.join(&b.label);
                std::fs::create_dir_all(&to).map_err(|e| e.to_string())?;
                b.mounted = Some(Some(to));
            }
            block_on(changed(conn, &at, "Filesystem"));
        }
        _ => return Err(format!("don't know {line:?}")),
    }
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let [_, address, root] = args.as_slice() else {
        eprintln!("usage: udisks-standin ADDRESS ROOT");
        std::process::exit(2);
    };
    let bus = Bus::connect(address).expect("the bus");
    let conn = bus.connection().clone();
    block_on(conn.object_server().at(ROOT, zbus::fdo::ObjectManager)).expect("the object manager");
    let state: Shared = Arc::new(Mutex::new(State { root: PathBuf::from(root), ..State::default() }));
    assert!(bus.own_name("org.freedesktop.UDisks2").expect("the name"), "someone else is udisks2 on this bus");
    println!("ok");
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        match command(&conn, &state, &line) {
            Ok(()) => println!("ok"),
            Err(e) => println!("error {e}"),
        }
    }
}
