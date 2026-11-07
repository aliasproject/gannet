//! The sidebar's places: home, the user's standard folders, Trash, and mounted disks.

use std::path::{Path, PathBuf};

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
    pub path: PathBuf,
    pub kind: PlaceKind,
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
    let mut places = vec![Place { label: "Home".into(), path: home.to_path_buf(), kind: PlaceKind::Home }];
    let text = std::fs::read_to_string(config_home.join("user-dirs.dirs")).unwrap_or_default();
    let dirs = parse_user_dirs(&text, home);
    let wanted = [("DOCUMENTS", "Documents", PlaceKind::Documents), ("DOWNLOAD", "Downloads", PlaceKind::Downloads), ("MUSIC", "Music", PlaceKind::Music), ("PICTURES", "Pictures", PlaceKind::Pictures), ("VIDEOS", "Videos", PlaceKind::Videos)];
    for (key, label, kind) in wanted {
        let path = dirs.iter().find(|(k, _)| k == key).map(|(_, p)| p.clone()).unwrap_or_else(|| home.join(label));
        if path != home && path.is_dir() {
            places.push(Place { label: label.into(), path, kind });
        }
    }
    places.push(Place { label: "Trash".into(), path: data_home.join("Trash/files"), kind: PlaceKind::Trash });
    places.push(Place { label: "Computer".into(), path: PathBuf::from("/"), kind: PlaceKind::Root });
    places
}

const NETWORK_FS: [&str; 5] = ["nfs", "nfs4", "cifs", "smb3", "sshfs"];

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
        out.push(Place { label, path: m.point.clone(), kind: PlaceKind::Disk });
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
}
