//! Drives through udisks2: which volumes there are, and mounting, unmounting, unlocking and
//! ejecting them. udisks2 does the work on the system bus, and polkit decides what's allowed,
//! asking for a password if the system says to.
//!
//! What udisks2 reports is read into a plain model first (`Snapshot`), so the rules for which
//! volumes show in the sidebar (`volumes`) are tested without a bus.
//!
//! The calls block until udisks2 answers, which can be as long as someone takes at polkit's
//! prompt, so run them off the UI thread. Each checks first that the drive is still the one that
//! was picked: a USB stick pulled out and another put in can take the same object path.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::time::Duration;

use zbus::{OwnedObjectPath, OwnedVariant, Variant};

use crate::bus::Bus;

pub const SERVICE: &str = "org.freedesktop.UDisks2";
pub const ROOT: &str = "/org/freedesktop/UDisks2";
const PREFIX: &str = "org.freedesktop.UDisks2.";
/// udisks2's way of saying "none" for an object path.
const NONE: &str = "/";

pub type Properties = HashMap<String, OwnedVariant>;
/// What `GetManagedObjects` answers: each object's interfaces and their properties.
pub type Objects = HashMap<OwnedObjectPath, HashMap<String, Properties>>;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Drive {
    pub path: String,
    /// Make, model and serial, as one string.
    pub id: String,
    pub size: u64,
    pub removable: bool,
    pub ejectable: bool,
    pub can_power_off: bool,
    /// How it's connected, like `usb` or `sdio`.
    pub connection: String,
    /// Microseconds since 1970 when the media went in, which changes when a card is swapped.
    pub media_detected: u64,
}

impl Drive {
    /// Whether it's meant to be taken out. Plenty of internal disks can be powered off, so
    /// that alone doesn't count.
    pub fn can_come_out(&self) -> bool {
        self.removable || self.ejectable || ["usb", "sdio", "ieee1394"].contains(&self.connection.as_str())
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Block {
    pub path: String,
    pub drive: String,
    pub label: String,
    /// What the desktop should call it, when it says.
    pub hint_name: String,
    pub id_usage: String,
    pub id_type: String,
    pub size: u64,
    pub hint_system: bool,
    pub hint_ignore: bool,
    /// For an unlocked volume's contents, the encrypted block they came from.
    pub crypto_backing: String,
    /// Where it's mounted, if it has a filesystem: empty when it isn't.
    pub mount_points: Option<Vec<PathBuf>>,
    /// For an encrypted block, its unlocked contents, or `None` while it's locked.
    pub encrypted: Option<Option<String>>,
}

/// Everything udisks2 knows about drives and the blocks on them, at one moment.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Snapshot {
    pub drives: BTreeMap<String, Drive>,
    pub blocks: BTreeMap<String, Block>,
}

/// What makes a drive the one that was picked, more than its path does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    pub id: String,
    pub size: u64,
    pub media_detected: u64,
}

/// A volume for the sidebar: a filesystem, or an encrypted volume that's still locked.
#[derive(Clone, Debug, PartialEq)]
pub struct Volume {
    /// The block to mount: the filesystem, or the locked encrypted block.
    pub block: String,
    pub drive: String,
    /// Its label, or `None` if it has none and something else has to name it.
    pub label: Option<String>,
    pub size: u64,
    pub mount: Option<PathBuf>,
    pub locked: bool,
    /// It's the contents of an encrypted volume, which unmounting locks again.
    pub unlocked: bool,
    /// Whether the drive can be taken out, so it gets an eject button.
    pub removable: bool,
    pub identity: Identity,
}

fn bool_of(p: &Properties, key: &str) -> bool {
    matches!(p.get(key).map(|v| &**v), Some(Variant::Bool(true)))
}

fn u64_of(p: &Properties, key: &str) -> u64 {
    match p.get(key).map(|v| &**v) {
        Some(Variant::U64(n)) => *n,
        _ => 0,
    }
}

fn string_of(p: &Properties, key: &str) -> String {
    match p.get(key).map(|v| &**v) {
        Some(Variant::Str(s)) => s.to_string(),
        Some(Variant::ObjectPath(o)) => o.to_string(),
        _ => String::new(),
    }
}

/// A byte string, as udisks2 gives paths: maybe ending in a zero byte.
fn bytes(v: &Variant) -> Option<Vec<u8>> {
    let Variant::Array(a) = v else { return None };
    let mut out: Vec<u8> = a.iter().map(|b| if let Variant::U8(b) = b { Some(*b) } else { None }).collect::<Option<_>>()?;
    while out.last() == Some(&0) {
        out.pop();
    }
    Some(out)
}

fn paths_of(p: &Properties, key: &str) -> Vec<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    let Some(Variant::Array(a)) = p.get(key).map(|v| &**v) else { return Vec::new() };
    a.iter().filter_map(bytes).filter(|b| !b.is_empty()).map(|b| PathBuf::from(std::ffi::OsString::from_vec(b))).collect()
}

impl Snapshot {
    pub fn from_objects(objects: &Objects) -> Self {
        let mut out = Self::default();
        for (path, interfaces) in objects {
            let path = path.to_string();
            let iface = |name: &str| interfaces.get(&format!("{PREFIX}{name}"));
            if let Some(d) = iface("Drive") {
                out.drives.insert(
                    path.clone(),
                    Drive {
                        path: path.clone(),
                        id: string_of(d, "Id"),
                        size: u64_of(d, "Size"),
                        removable: bool_of(d, "Removable") || bool_of(d, "MediaRemovable"),
                        ejectable: bool_of(d, "Ejectable"),
                        can_power_off: bool_of(d, "CanPowerOff"),
                        connection: string_of(d, "ConnectionBus"),
                        media_detected: u64_of(d, "TimeMediaDetected"),
                    },
                );
            }
            if let Some(b) = iface("Block") {
                let mount_points = iface("Filesystem").map(|f| paths_of(f, "MountPoints"));
                let encrypted = iface("Encrypted").map(|e| Some(string_of(e, "CleartextDevice")).filter(|c| !c.is_empty() && c != NONE));
                out.blocks.insert(
                    path.clone(),
                    Block {
                        path,
                        drive: string_of(b, "Drive"),
                        label: string_of(b, "IdLabel"),
                        hint_name: string_of(b, "HintName"),
                        id_usage: string_of(b, "IdUsage"),
                        id_type: string_of(b, "IdType"),
                        size: u64_of(b, "Size"),
                        hint_system: bool_of(b, "HintSystem"),
                        hint_ignore: bool_of(b, "HintIgnore"),
                        crypto_backing: string_of(b, "CryptoBackingDevice"),
                        mount_points,
                        encrypted,
                    },
                );
            }
        }
        out
    }

    /// The block a volume really lives on: an unlocked volume's contents live on the encrypted
    /// block, which is the one on a drive.
    fn underlying<'a>(&'a self, block: &'a Block) -> &'a Block {
        match self.blocks.get(&block.crypto_backing) {
            Some(backing) if block.crypto_backing != NONE => backing,
            _ => block,
        }
    }

    /// The volumes to show, in a steady order: each filesystem, and each encrypted volume while
    /// it's locked, on a drive that isn't the computer's own. Once unlocked, an encrypted volume
    /// shows as what's inside it.
    pub fn volumes(&self) -> Vec<Volume> {
        let mut out = Vec::new();
        for block in self.blocks.values() {
            let underlying = self.underlying(block);
            let Some(drive) = self.drives.get(&underlying.drive) else { continue };
            let locked = block.encrypted == Some(None);
            let shown = match (&block.mount_points, &block.encrypted) {
                (Some(_), _) => block.id_usage == "filesystem",
                (None, Some(None)) => block.id_usage == "crypto",
                _ => false,
            };
            if !shown || block.hint_ignore || underlying.hint_system || underlying.hint_ignore {
                continue;
            }
            let label = [&block.label, &block.hint_name, &underlying.label, &underlying.hint_name].into_iter().find(|l| !l.is_empty()).cloned();
            out.push(Volume {
                block: block.path.clone(),
                drive: drive.path.clone(),
                label,
                size: if block.size > 0 { block.size } else { underlying.size },
                mount: block.mount_points.as_ref().and_then(|m| m.first().cloned()),
                locked,
                unlocked: block.crypto_backing != NONE && !block.crypto_backing.is_empty(),
                removable: drive.can_come_out(),
                identity: Identity { id: drive.id.clone(), size: drive.size, media_detected: drive.media_detected },
            });
        }
        out
    }

    /// Whether the drive at `drive` is still the one `identity` describes.
    pub fn same_drive(&self, drive: &str, identity: &Identity) -> bool {
        self.drives.get(drive).is_some_and(|d| d.id == identity.id && d.size == identity.size && d.media_detected == identity.media_detected)
    }

    /// The blocks on a drive, including unlocked contents of encrypted ones.
    fn on_drive(&self, drive: &str) -> Vec<&Block> {
        self.blocks.values().filter(|b| self.underlying(b).drive == drive).collect()
    }
}

/// Why something couldn't be done, in words the window can show.
#[derive(Clone, Debug, PartialEq)]
pub enum DiskError {
    /// The system doesn't allow it, or the password asked for wasn't given.
    NotAuthorized,
    /// Something has files on it open.
    Busy,
    WrongPassphrase,
    /// It's been taken out, or another drive is there now.
    Changed,
    /// udisks2 isn't running, or didn't answer.
    NoService,
    Other(String),
}

impl std::fmt::Display for DiskError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotAuthorized => write!(f, "the system didn't allow it"),
            Self::Busy => write!(f, "files on it are in use"),
            Self::WrongPassphrase => write!(f, "that passphrase isn't right"),
            Self::Changed => write!(f, "it's been taken out, or swapped for another"),
            Self::NoService => write!(f, "udisks2 isn't running"),
            Self::Other(why) => write!(f, "{why}"),
        }
    }
}

/// udisks2's errors, by name, in words. `unlocking` is for errors from `Unlock`, where a plain
/// failure nearly always means the passphrase.
fn disk_error(e: zbus::Error, unlocking: bool) -> DiskError {
    match &e {
        zbus::Error::MethodError(name, message, _) => {
            let name = name.as_str();
            let message = message.clone().unwrap_or_default();
            match name.strip_prefix("org.freedesktop.UDisks2.Error.") {
                Some(n) if n.starts_with("NotAuthorized") => DiskError::NotAuthorized,
                Some("DeviceBusy") => DiskError::Busy,
                Some("Failed") if unlocking && (message.contains("passphrase") || message.contains("not permitted") || message.contains("No key available")) => DiskError::WrongPassphrase,
                // Unmounting something busy can come back as a plain failure.
                Some("Failed") if message.contains("target is busy") || message.contains("is busy") => DiskError::Busy,
                _ if name.starts_with("org.freedesktop.DBus.Error.ServiceUnknown") || name.starts_with("org.freedesktop.DBus.Error.NameHasNoOwner") => DiskError::NoService,
                _ if name == "org.freedesktop.DBus.Error.UnknownObject" || name == "org.freedesktop.DBus.Error.UnknownMethod" => DiskError::Changed,
                _ => DiskError::Other(if message.is_empty() { name.to_string() } else { message }),
            }
        }
        zbus::Error::InputOutput(_) | zbus::Error::Handshake(_) | zbus::Error::Address(_) => DiskError::NoService,
        _ => DiskError::Other(e.to_string()),
    }
}

/// Options for each call: polkit may ask the user, rather than just saying no.
fn options() -> HashMap<&'static str, Variant<'static>> {
    HashMap::from([("auth.no_user_interaction", Variant::from(false))])
}

/// udisks2 on one bus connection, for calls that may wait on a person at polkit's prompt.
pub struct Udisks {
    bus: Bus,
}

/// Long enough for someone to answer polkit's password prompt.
pub const ACTION_LIMIT: Duration = Duration::from_secs(600);

impl Udisks {
    pub fn new(bus: Bus) -> Self {
        Self { bus }
    }

    pub fn connect(address: &str) -> Result<Self, DiskError> {
        Bus::connect_with_limit(address, ACTION_LIMIT).map(Self::new).map_err(|e| disk_error(e, false))
    }

    pub fn bus(&self) -> &Bus {
        &self.bus
    }

    pub fn snapshot(&self) -> Result<Snapshot, DiskError> {
        let objects: Objects = self.bus.call(SERVICE, ROOT, "org.freedesktop.DBus.ObjectManager", "GetManagedObjects", &()).map_err(|e| disk_error(e, false))?;
        Ok(Snapshot::from_objects(&objects))
    }

    /// The snapshot, once it's been checked that `volume`'s drive is still the one it was.
    fn checked(&self, volume: &Volume) -> Result<Snapshot, DiskError> {
        let now = self.snapshot()?;
        if !now.same_drive(&volume.drive, &volume.identity) || !now.blocks.contains_key(&volume.block) {
            return Err(DiskError::Changed);
        }
        Ok(now)
    }

    fn call<R>(&self, path: &str, interface: &str, method: &str, body: &(impl serde::Serialize + zbus::zvariant::DynamicType), unlocking: bool) -> Result<R, DiskError>
    where
        R: for<'d> zbus::zvariant::DynamicDeserialize<'d>,
    {
        self.bus.call(SERVICE, path, &format!("{PREFIX}{interface}"), method, body).map_err(|e| disk_error(e, unlocking))
    }

    /// Mounts a block, and says where. If something else, like an automounter, got there
    /// first, that's where it is.
    fn mount_block(&self, block: &str) -> Result<PathBuf, DiskError> {
        match self.call::<String>(block, "Filesystem", "Mount", &(options(),), false) {
            Ok(at) => Ok(PathBuf::from(at)),
            Err(e) => match self.snapshot().ok().and_then(|s| s.blocks.get(block)?.mount_points.as_ref()?.first().cloned()) {
                Some(at) => Ok(at),
                None => Err(e),
            },
        }
    }

    /// Mounts a volume that isn't locked, and says where.
    pub fn mount(&self, volume: &Volume) -> Result<PathBuf, DiskError> {
        let now = self.checked(volume)?;
        if let Some(at) = now.blocks.get(&volume.block).and_then(|b| b.mount_points.as_ref()?.first().cloned()) {
            return Ok(at);
        }
        self.mount_block(&volume.block)
    }

    /// Unlocks an encrypted volume with its passphrase and mounts what's inside, and says where.
    pub fn unlock(&self, volume: &Volume, passphrase: &str) -> Result<PathBuf, DiskError> {
        self.checked(volume)?;
        let cleartext: OwnedObjectPath = self.call(&volume.block, "Encrypted", "Unlock", &(passphrase, options()), true)?;
        // The contents' filesystem can take a moment to show up after the unlock returns.
        let mut tries = 0;
        loop {
            match self.mount_block(cleartext.as_str()) {
                Err(DiskError::Changed) if tries < 20 => {
                    tries += 1;
                    std::thread::sleep(Duration::from_millis(50));
                }
                // Something other than a filesystem inside, like LVM: it's unlocked, but there's
                // nothing here to open.
                Err(DiskError::Changed) => return Err(DiskError::Other("it's unlocked, but there's nothing on it that can be opened".into())),
                other => return other,
            }
        }
    }

    /// Unmounts a volume, and locks it again if it was encrypted.
    pub fn unmount(&self, volume: &Volume) -> Result<(), DiskError> {
        let now = self.checked(volume)?;
        let Some(block) = now.blocks.get(&volume.block) else { return Err(DiskError::Changed) };
        if block.mount_points.as_ref().is_some_and(|m| !m.is_empty()) {
            self.call::<()>(&block.path, "Filesystem", "Unmount", &(options(),), false)?;
        }
        if block.crypto_backing != NONE && now.blocks.contains_key(&block.crypto_backing) {
            self.call::<()>(&block.crypto_backing, "Encrypted", "Lock", &(options(),), false)?;
        }
        Ok(())
    }

    /// Unmounts everything on the volume's drive, locks what's encrypted, then powers the drive
    /// off if it can be, or ejects it.
    pub fn eject(&self, volume: &Volume) -> Result<(), DiskError> {
        let now = self.checked(volume)?;
        let blocks = now.on_drive(&volume.drive);
        // What's inside an encrypted volume comes out first, then the volume can be locked.
        let mut order: Vec<&Block> = blocks.clone();
        order.sort_by_key(|b| b.crypto_backing == NONE);
        for b in &order {
            if b.mount_points.as_ref().is_some_and(|m| !m.is_empty()) {
                self.call::<()>(&b.path, "Filesystem", "Unmount", &(options(),), false)?;
            }
        }
        for b in &blocks {
            if matches!(b.encrypted, Some(Some(_))) {
                self.call::<()>(&b.path, "Encrypted", "Lock", &(options(),), false)?;
            }
        }
        let Some(drive) = now.drives.get(&volume.drive) else { return Err(DiskError::Changed) };
        // A drive with a tray or slot ejects its media. Anything else is powered off, so it's
        // safe to pull out.
        if drive.ejectable {
            self.call::<()>(&drive.path, "Drive", "Eject", &(options(),), false)
        } else if drive.can_power_off {
            self.call::<()>(&drive.path, "Drive", "PowerOff", &(options(),), false)
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(value: Variant<'static>) -> OwnedVariant {
        value.try_into().unwrap()
    }

    fn props(list: Vec<(&str, OwnedVariant)>) -> Properties {
        list.into_iter().map(|(k, val)| (k.to_string(), val)).collect()
    }

    fn object(objects: &mut Objects, path: &str, interfaces: Vec<(&str, Properties)>) {
        let path = OwnedObjectPath::try_from(format!("{ROOT}/{path}")).unwrap();
        objects.insert(path, interfaces.into_iter().map(|(i, p)| (format!("{PREFIX}{i}"), p)).collect());
    }

    fn drive(objects: &mut Objects, name: &str, id: &str, removable: bool) {
        // Internal disks can often be powered off too, which doesn't make them removable.
        object(objects, &format!("drives/{name}"), vec![("Drive", props(vec![("Id", v(Variant::from(id.to_string()))), ("Size", v(Variant::U64(32_000_000_000))), ("Removable", v(Variant::Bool(removable))), ("CanPowerOff", v(Variant::Bool(true))), ("ConnectionBus", v(Variant::from(if removable { "usb" } else { "" }.to_string()))), ("TimeMediaDetected", v(Variant::U64(1000)))]))]);
    }

    fn block(name: &str, drive: &str, label: &str, usage: &str, hint_system: bool) -> Properties {
        let drive_path = if drive == "/" { "/".to_string() } else { format!("{ROOT}/drives/{drive}") };
        props(vec![
            ("Drive", v(Variant::from(OwnedObjectPath::try_from(drive_path).unwrap()))),
            ("IdLabel", v(Variant::from(label.to_string()))),
            ("IdUsage", v(Variant::from(usage.to_string()))),
            ("Size", v(Variant::U64(16_000_000_000))),
            ("HintSystem", v(Variant::Bool(hint_system))),
            ("HintIgnore", v(Variant::Bool(false))),
            ("CryptoBackingDevice", v(Variant::from(OwnedObjectPath::try_from(if name.starts_with("dm") { format!("{ROOT}/block_devices/sdb2") } else { "/".to_string() }).unwrap()))),
        ])
    }

    fn mounts(points: &[&str]) -> Properties {
        let list: Vec<Vec<u8>> = points.iter().map(|p| [p.as_bytes(), b"\0"].concat()).collect();
        props(vec![("MountPoints", v(Variant::from(list)))])
    }

    /// A machine with its own disk, a USB stick with two partitions (one mounted), and an
    /// encrypted partition, as `unlocked` says.
    fn machine(unlocked: bool) -> Objects {
        let mut o = Objects::new();
        drive(&mut o, "Internal", "Samsung-SSD", false);
        drive(&mut o, "Stick", "Kingston-DataTraveler-1234", true);
        object(&mut o, "block_devices/nvme0n1p1", vec![("Block", block("nvme0n1p1", "Internal", "", "filesystem", true)), ("Filesystem", mounts(&["/boot"]))]);
        object(&mut o, "block_devices/sdb", vec![("Block", block("sdb", "Stick", "", "", false)), ("PartitionTable", props(vec![]))]);
        object(&mut o, "block_devices/sdb1", vec![("Block", block("sdb1", "Stick", "PHOTOS", "filesystem", false)), ("Filesystem", mounts(&["/run/media/me/PHOTOS"]))]);
        let cleartext = if unlocked { format!("{ROOT}/block_devices/dm_2d1") } else { "/".to_string() };
        object(&mut o, "block_devices/sdb2", vec![("Block", block("sdb2", "Stick", "", "crypto", false)), ("Encrypted", props(vec![("CleartextDevice", v(Variant::from(OwnedObjectPath::try_from(cleartext).unwrap())))]))]);
        object(&mut o, "block_devices/sdb3", vec![("Block", block("sdb3", "Stick", "SPARE", "filesystem", false)), ("Filesystem", mounts(&[]))]);
        if unlocked {
            object(&mut o, "block_devices/dm_2d1", vec![("Block", block("dm_2d1", "/", "SECRETS", "filesystem", false)), ("Filesystem", mounts(&["/run/media/me/SECRETS"]))]);
        }
        object(&mut o, "block_devices/loop0", vec![("Block", block("loop0", "/", "snap", "filesystem", false)), ("Filesystem", mounts(&["/snap/x"]))]);
        o
    }

    #[test]
    fn the_sidebar_gets_removable_volumes_and_not_the_system() {
        let s = Snapshot::from_objects(&machine(false));
        let volumes = s.volumes();
        let shown: Vec<(Option<&str>, Option<&str>, bool, bool)> = volumes.iter().map(|v| (v.label.as_deref(), v.mount.as_ref().and_then(|m| m.to_str()), v.locked, v.removable)).collect();
        assert_eq!(shown, [(Some("PHOTOS"), Some("/run/media/me/PHOTOS"), false, true), (None, None, true, true), (Some("SPARE"), None, false, true)]);
        let photos = &s.volumes()[0];
        assert_eq!(photos.identity, Identity { id: "Kingston-DataTraveler-1234".into(), size: 32_000_000_000, media_detected: 1000 });
        assert!(photos.block.ends_with("/sdb1") && photos.drive.ends_with("/drives/Stick"));
    }

    #[test]
    fn an_internal_disk_that_can_power_off_gets_no_eject_button() {
        let mut o = machine(false);
        drive(&mut o, "Data", "WD-Blue", false);
        object(&mut o, "block_devices/sdc1", vec![("Block", block("sdc1", "Data", "DATA", "filesystem", false)), ("Filesystem", mounts(&["/run/media/me/DATA"]))]);
        let s = Snapshot::from_objects(&o);
        let data = s.volumes().into_iter().find(|v| v.label.as_deref() == Some("DATA")).unwrap();
        assert!(!data.removable, "shown, as udisks2 doesn't call it the system's, but it doesn't come out");
        assert!(s.volumes().iter().filter(|v| v.label.as_deref() != Some("DATA")).all(|v| v.removable), "the USB stick does");
    }

    #[test]
    fn an_unlocked_volume_shows_as_what_is_inside() {
        let s = Snapshot::from_objects(&machine(true));
        let volumes = s.volumes();
        let labels: Vec<Option<&str>> = volumes.iter().map(|v| v.label.as_deref()).collect();
        assert_eq!(labels, [Some("SECRETS"), Some("PHOTOS"), Some("SPARE")], "the locked one is gone, and its contents show");
        let secrets = &s.volumes()[0];
        assert!(secrets.drive.ends_with("/drives/Stick"), "on the stick it's really on");
        assert!(!secrets.locked && secrets.removable && secrets.unlocked);
        assert!(!s.volumes()[1].unlocked, "a plain one isn't");
        assert_eq!(s.blocks[&format!("{ROOT}/block_devices/sdb2")].encrypted, Some(Some(format!("{ROOT}/block_devices/dm_2d1"))));
    }

    #[test]
    fn the_same_drive_is_more_than_the_same_path() {
        let before = Snapshot::from_objects(&machine(false));
        let picked = before.volumes()[0].clone();
        assert!(before.same_drive(&picked.drive, &picked.identity));
        let mut swapped = machine(false);
        drive(&mut swapped, "Stick", "SanDisk-Ultra-9999", true);
        assert!(!Snapshot::from_objects(&swapped).same_drive(&picked.drive, &picked.identity), "another stick at the same path");
        let mut gone = machine(false);
        gone.retain(|p, _| !p.as_str().ends_with("/Stick"));
        assert!(!Snapshot::from_objects(&gone).same_drive(&picked.drive, &picked.identity));
    }

    #[test]
    fn udisks_errors_become_words() {
        let err = |name: &str, message: &str| zbus::Error::MethodError(name.try_into().unwrap(), Some(message.to_string()), zbus::message::Message::method_call("/", "x").build(&()).unwrap());
        assert_eq!(disk_error(err("org.freedesktop.UDisks2.Error.NotAuthorizedCanObtain", ""), false), DiskError::NotAuthorized);
        assert_eq!(disk_error(err("org.freedesktop.UDisks2.Error.DeviceBusy", "busy"), false), DiskError::Busy);
        assert_eq!(disk_error(err("org.freedesktop.UDisks2.Error.Failed", "Error unmounting: target is busy"), false), DiskError::Busy);
        assert_eq!(disk_error(err("org.freedesktop.UDisks2.Error.Failed", "Failed to activate device: Incorrect passphrase."), true), DiskError::WrongPassphrase);
        assert_eq!(disk_error(err("org.freedesktop.UDisks2.Error.Failed", "Failed to activate device: Incorrect passphrase."), false), DiskError::Other("Failed to activate device: Incorrect passphrase.".into()));
        assert_eq!(disk_error(err("org.freedesktop.DBus.Error.ServiceUnknown", ""), false), DiskError::NoService);
        assert_eq!(disk_error(err("org.freedesktop.DBus.Error.UnknownObject", ""), false), DiskError::Changed);
    }

    /// The stand-in udisks2 (`examples/udisks-standin.rs`) on a bus of its own, which `cargo
    /// test` builds beside the tests.
    struct StandIn {
        child: std::process::Child,
        input: std::process::ChildStdin,
        output: std::io::BufReader<std::process::ChildStdout>,
        bus: crate::testutil::PrivateBus,
        root: crate::testutil::Scratch,
    }

    impl StandIn {
        fn start() -> Self {
            use std::io::BufRead;
            let exe = std::env::current_exe().unwrap().parent().unwrap().parent().unwrap().join("examples/udisks-standin");
            let bus = crate::testutil::PrivateBus::start();
            let root = crate::testutil::Scratch::new();
            let mut child = std::process::Command::new(&exe).args([&bus.address, root.path().to_str().unwrap()]).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).spawn().unwrap_or_else(|e| panic!("{}: {e}", exe.display()));
            let input = child.stdin.take().unwrap();
            let mut output = std::io::BufReader::new(child.stdout.take().unwrap());
            let mut line = String::new();
            output.read_line(&mut line).unwrap();
            assert_eq!(line.trim(), "ok", "the stand-in started");
            Self { child, input, output, bus, root }
        }

        fn say(&mut self, command: &str) {
            use std::io::{BufRead, Write};
            writeln!(self.input, "{command}").unwrap();
            let mut line = String::new();
            self.output.read_line(&mut line).unwrap();
            assert_eq!(line.trim(), "ok", "{command}");
        }
    }

    impl Drop for StandIn {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    #[test]
    fn mounting_unlocking_and_ejecting_through_udisks() {
        assert!(std::env::var_os("GANNET_TEST_SANDBOX").is_some(), "run tests with scripts/test.sh");
        let mut standin = StandIn::start();
        standin.say("drive Stick Kingston-1234");
        standin.say("fs sdb1 Stick PHOTOS");
        standin.say("luks sdb2 Stick hunter2 SECRETS");
        let udisks = Udisks::connect(&standin.bus.address).unwrap();
        let volumes = udisks.snapshot().unwrap().volumes();
        let labels: Vec<(Option<&str>, bool)> = volumes.iter().map(|v| (v.label.as_deref(), v.locked)).collect();
        assert_eq!(labels, [(Some("PHOTOS"), false), (None, true)]);
        let (photos, secret) = (volumes[0].clone(), volumes[1].clone());
        // Mounting says where, and the snapshot agrees.
        let at = udisks.mount(&photos).unwrap();
        assert_eq!(at, standin.root.path().join("PHOTOS"));
        assert_eq!(udisks.snapshot().unwrap().volumes()[0].mount.as_ref(), Some(&at));
        // Busy, then not.
        standin.say("busy sdb1");
        assert_eq!(udisks.unmount(&photos), Err(DiskError::Busy));
        standin.say("idle sdb1");
        udisks.unmount(&photos).unwrap();
        assert_eq!(udisks.snapshot().unwrap().volumes()[0].mount, None);
        // A wrong passphrase, then the right one, which mounts what's inside.
        assert_eq!(udisks.unlock(&secret, "nope"), Err(DiskError::WrongPassphrase));
        let inside = udisks.unlock(&secret, "hunter2").unwrap();
        assert_eq!(inside, standin.root.path().join("SECRETS"));
        let now = udisks.snapshot().unwrap().volumes();
        let shown: Vec<(Option<&str>, bool)> = now.iter().map(|v| (v.label.as_deref(), v.mount.is_some())).collect();
        assert!(shown.contains(&(Some("SECRETS"), true)) && !now.iter().any(|v| v.locked), "unlocked shows as what's inside: {shown:?}");
        // Unmounting the contents locks the volume again.
        let secrets = now.iter().find(|v| v.label.as_deref() == Some("SECRETS")).unwrap().clone();
        udisks.unmount(&secrets).unwrap();
        assert!(udisks.snapshot().unwrap().volumes().iter().any(|v| v.locked), "locked again");
        // Ejecting unmounts and locks everything on the drive, then it's gone.
        udisks.mount(&photos).unwrap();
        udisks.unlock(&secret, "hunter2").unwrap();
        udisks.eject(&photos).unwrap();
        let mut tries = 0;
        while !udisks.snapshot().unwrap().volumes().is_empty() && tries < 100 {
            tries += 1;
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(udisks.snapshot().unwrap().volumes().is_empty(), "powered off and gone");
    }

    #[test]
    fn something_else_mounting_it_first_still_says_where_it_is() {
        assert!(std::env::var_os("GANNET_TEST_SANDBOX").is_some(), "run tests with scripts/test.sh");
        let mut standin = StandIn::start();
        standin.say("drive Stick Kingston-1234");
        standin.say("fs sdb1 Stick PHOTOS");
        let udisks = Udisks::connect(&standin.bus.address).unwrap();
        // An automounter gets there between the look and the mount.
        standin.say("mount sdb1");
        let at = udisks.mount_block(&format!("{ROOT}/block_devices/sdb1")).unwrap();
        assert_eq!(at, standin.root.path().join("PHOTOS"));
    }

    #[test]
    fn a_different_stick_at_the_same_place_is_left_alone() {
        assert!(std::env::var_os("GANNET_TEST_SANDBOX").is_some(), "run tests with scripts/test.sh");
        let mut standin = StandIn::start();
        standin.say("drive Stick Kingston-1234");
        standin.say("fs sdb1 Stick PHOTOS");
        let udisks = Udisks::connect(&standin.bus.address).unwrap();
        let picked = udisks.snapshot().unwrap().volumes()[0].clone();
        standin.say("unplug Stick");
        std::thread::sleep(Duration::from_millis(100));
        standin.say("drive Stick SanDisk-9999");
        standin.say("fs sdb1 Stick OTHER");
        assert_eq!(udisks.mount(&picked), Err(DiskError::Changed));
        assert_eq!(udisks.eject(&picked), Err(DiskError::Changed));
        assert!(udisks.snapshot().unwrap().volumes()[0].mount.is_none(), "nothing was done to the other stick");
    }
}
