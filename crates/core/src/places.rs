//! The sidebar's places: home, the user's standard folders, Trash, and mounted disks.

use std::path::{Path, PathBuf};

use crate::disks::Volume;
use crate::mounts::Mount;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaceKind {
    Home,
    Documents,
    Downloads,
    Music,
    Pictures,
    Videos,
    Trash,
    Root,
    Disk,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Place {
    pub label: String,
    /// Where it opens. Empty for a drive that isn't mounted yet.
    pub path: PathBuf,
    pub kind: PlaceKind,
    /// The drive's volume, for one udisks2 knows about, to mount, unlock or eject.
    pub volume: Option<Volume>,
}

/// Reads `user-dirs.dirs` text: lines like `XDG_MUSIC_DIR="$HOME/Music"`.
pub fn parse_user_dirs(text: &str, home: &Path) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else { continue };
        let Some(name) = key.trim().strip_prefix("XDG_").and_then(|k| k.strip_suffix("_DIR")) else { continue };
        let value = value.trim().trim_matches('"');
        let path = match value.strip_prefix("$HOME") {
            Some(rest) => home.join(rest.trim_start_matches('/')),
            None if value.starts_with('/') => PathBuf::from(value),
            None => continue,
        };
        out.push((name.to_string(), path));
    }
    out
}

/// The standard places, skipping folders that don't exist or just point back at home,
/// as aliasOS does for Desktop and Templates.
pub fn standard_places(home: &Path, config_home: &Path, data_home: &Path) -> Vec<Place> {
    let mut places = vec![Place { label: "Home".into(), path: home.to_path_buf(), kind: PlaceKind::Home, volume: None }];
    let text = std::fs::read_to_string(config_home.join("user-dirs.dirs")).unwrap_or_default();
    let dirs = parse_user_dirs(&text, home);
    let wanted = [("DOCUMENTS", "Documents", PlaceKind::Documents), ("DOWNLOAD", "Downloads", PlaceKind::Downloads), ("MUSIC", "Music", PlaceKind::Music), ("PICTURES", "Pictures", PlaceKind::Pictures), ("VIDEOS", "Videos", PlaceKind::Videos)];
    for (key, label, kind) in wanted {
        let path = dirs.iter().find(|(k, _)| k == key).map(|(_, p)| p.clone()).unwrap_or_else(|| home.join(label));
        if path != home && path.is_dir() {
            places.push(Place { label: label.into(), path, kind, volume: None });
        }
    }
    places.push(Place { label: "Trash".into(), path: data_home.join("Trash/files"), kind: PlaceKind::Trash, volume: None });
    places.push(Place { label: "Computer".into(), path: PathBuf::from("/"), kind: PlaceKind::Root, volume: None });
    places
}

const NETWORK_FS: [&str; 5] = ["nfs", "nfs4", "cifs", "smb3", "sshfs"];

/// Drives udisks2 knows about, by their label, or by `unnamed` for one without.
pub fn volume_places(volumes: &[Volume], unnamed: impl Fn(&Volume) -> String) -> Vec<Place> {
    volumes.iter().map(|v| Place { label: v.label.clone().unwrap_or_else(|| unnamed(v)), path: v.mount.clone().unwrap_or_default(), kind: PlaceKind::Disk, volume: Some(v.clone()) }).collect()
}

/// The sidebar's disks: what udisks2 knows, then anything else mounted that it doesn't, like a
/// network share.
pub fn all_disk_places(volumes: &[Volume], mounts: &[Mount], unnamed: impl Fn(&Volume) -> String) -> Vec<Place> {
    let mut out = volume_places(volumes, unnamed);
    let known: Vec<PathBuf> = volumes.iter().filter_map(|v| v.mount.clone()).collect();
    out.extend(disk_places(mounts).into_iter().filter(|p| !known.contains(&p.path)));
    out
}

/// Mounted disks worth showing: real devices and network shares, not system internals.
pub fn disk_places(mounts: &[Mount]) -> Vec<Place> {
    let mut seen: Vec<&str> = Vec::new();
    let mut out = Vec::new();
    for m in mounts {
        let point = m.point.to_string_lossy();
        let real = m.source.starts_with("/dev/") || NETWORK_FS.contains(&m.fs_type.as_str()) || m.fs_type.starts_with("fuse.");
        let internal = ["/", "/home", "/usr", "/var", "/nix", "/etc", "/opt", "/srv", "/root"].contains(&point.as_ref()) || ["/boot", "/efi", "/proc", "/sys", "/dev", "/run/user", "/var/", "/snap", "/tmp"].iter().any(|p| point.starts_with(p));
        // A bind mount or a btrfs subvolume repeats its device, so any device already seen,
        // including a system one, doesn't count again.
        let repeat = seen.contains(&m.source.as_str());
        seen.push(&m.source);
        if !real || internal || repeat {
            continue;
        }
        let label = m.point.file_name().map_or_else(|| point.to_string(), |n| n.to_string_lossy().into_owned());
        out.push(Place { label, path: m.point.clone(), kind: PlaceKind::Disk, volume: None });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mounts::parse_mountinfo;
    use crate::testutil::Scratch;

    #[test]
    fn user_dirs_parse() {
        let d = parse_user_dirs("# c\nXDG_MUSIC_DIR=\"$HOME/Tunes\"\nXDG_DESKTOP_DIR=\"$HOME/\"\nXDG_X_DIR=\"/abs\"\nnoise\nXDG_BAD_DIR=\"relative\"\n", Path::new("/h"));
        assert_eq!(d, vec![("MUSIC".into(), "/h/Tunes".into()), ("DESKTOP".into(), "/h/".into()), ("X".into(), "/abs".into())]);
    }

    #[test]
    fn standard_places_skip_missing_and_home_itself() {
        let s = Scratch::new();
        let home = s.dir("home");
        s.dir("home/Tunes");
        s.dir("home/Documents");
        s.file("config/user-dirs.dirs", b"XDG_MUSIC_DIR=\"$HOME/Tunes\"\nXDG_VIDEOS_DIR=\"$HOME/\"\nXDG_PICTURES_DIR=\"$HOME/Gone\"\n");
        let p = standard_places(&home, &s.path().join("config"), &s.path().join("data"));
        let labels: Vec<&str> = p.iter().map(|x| x.label.as_str()).collect();
        assert_eq!(labels, ["Home", "Documents", "Music", "Trash", "Computer"]);
        assert_eq!(p[2].path, home.join("Tunes"));
        assert_eq!(p[3].path, s.path().join("data/Trash/files"));
    }

    #[test]
    fn disks_skip_system_mounts_and_repeats() {
        let m = parse_mountinfo(
            b"1 0 0:1 / / rw - btrfs /dev/mapper/root rw\n\
2 1 0:1 / /home rw - btrfs /dev/mapper/root rw\n\
3 1 8:1 / /boot rw - vfat /dev/sda1 rw\n\
4 1 0:5 / /proc rw - proc proc rw\n\
5 1 8:17 / /run/media/me/USB\\040Disk rw - exfat /dev/sdb1 rw\n\
6 1 8:17 / /mnt/again rw - exfat /dev/sdb1 rw\n\
9 1 0:1 / /home/me/bound rw - btrfs /dev/mapper/root rw\n\
7 1 0:9 / /mnt/share rw - nfs4 host:/x rw\n\
8 1 0:9 / /run/user/1000/doc rw - fuse.portal portal rw\n",
        );
        let d = disk_places(&m);
        let labels: Vec<&str> = d.iter().map(|x| x.label.as_str()).collect();
        assert_eq!(labels, ["USB Disk", "share"]);
    }

    #[test]
    fn udisks_drives_come_first_and_mounts_it_knows_arent_repeated() {
        use crate::disks::Identity;
        let m = parse_mountinfo(b"5 1 8:17 / /run/media/me/PHOTOS rw - exfat /dev/sdb1 rw\n7 1 0:9 / /mnt/share rw - nfs4 host:/x rw\n");
        let identity = Identity { id: "stick".into(), size: 1, media_detected: 1 };
        let volume = |label: Option<&str>, mount: Option<&str>| Volume { block: "b".into(), drive: "d".into(), label: label.map(String::from), size: 8_000_000_000, mount: mount.map(PathBuf::from), locked: false, unlocked: false, removable: true, identity: identity.clone() };
        let volumes = [volume(Some("PHOTOS"), Some("/run/media/me/PHOTOS")), volume(None, None)];
        let places = all_disk_places(&volumes, &m, |v| format!("{} GB Volume", v.size / 1_000_000_000));
        let shown: Vec<(&str, &str, bool)> = places.iter().map(|p| (p.label.as_str(), p.path.to_str().unwrap(), p.volume.is_some())).collect();
        assert_eq!(shown, [("PHOTOS", "/run/media/me/PHOTOS", true), ("8 GB Volume", "", true), ("share", "/mnt/share", false)]);
    }
}
