//! What's mounted, from `/proc/self/mountinfo`.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq)]
pub struct Mount {
    pub point: PathBuf,
    pub fs_type: String,
    pub source: String,
}

pub fn read_mounts() -> Vec<Mount> {
    std::fs::read("/proc/self/mountinfo").map(|b| parse_mountinfo(&b)).unwrap_or_default()
}

/// Lines look like: `36 35 98:0 /root /mnt rw,noatime master:1 - ext3 /dev/root rw`.
pub fn parse_mountinfo(text: &[u8]) -> Vec<Mount> {
    let mut out = Vec::new();
    for line in text.split(|&b| b == b'\n') {
        let fields: Vec<&[u8]> = line.split(|&b| b == b' ').collect();
        let Some(sep) = fields.iter().position(|f| *f == b"-") else { continue };
        if fields.len() < 5 || sep + 2 >= fields.len() {
            continue;
        }
        out.push(Mount {
            point: PathBuf::from(OsStr::from_bytes(&unescape(fields[4]))),
            fs_type: String::from_utf8_lossy(fields[sep + 1]).into_owned(),
            source: String::from_utf8_lossy(&unescape(fields[sep + 2])).into_owned(),
        });
    }
    out
}

/// Mount points escape spaces and such as `\040`.
fn unescape(s: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    let mut rest = s;
    loop {
        match rest {
            [b'\\', a @ b'0'..=b'7', b @ b'0'..=b'7', c @ b'0'..=b'7', after @ ..] => {
                out.push(((a - b'0') << 6) | ((b - b'0') << 3) | (c - b'0'));
                rest = after;
            }
            [byte, after @ ..] => {
                out.push(*byte);
                rest = after;
            }
            [] => return out,
        }
    }
}

/// The mount point holding `path`, which should already be canonical.
pub fn mount_point_of(mounts: &[Mount], path: &Path) -> Option<PathBuf> {
    mounts.iter().filter(|m| path.starts_with(&m.point)).max_by_key(|m| m.point.as_os_str().len()).map(|m| m.point.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &[u8] = b"36 35 98:0 / / rw - ext4 /dev/sda1 rw\n\
37 36 98:1 / /home rw shared:1 - btrfs /dev/mapper/root rw\n\
38 36 98:2 / /run/media/me/My\\040Disk rw nosuid - exfat /dev/sdb1 rw\n\
garbage line\n";

    #[test]
    fn unescapes_only_three_octal_digits() {
        assert_eq!(unescape(b"a\\040b"), b"a b");
        assert_eq!(unescape(b"end\\040"), b"end ");
        assert_eq!(unescape(b"\\134"), b"\\");
        assert_eq!(unescape(b"\\777"), [255]);
        assert_eq!(unescape(b"short\\04"), b"short\\04");
        assert_eq!(unescape(b"\\048"), b"\\048");
    }

    #[test]
    fn parses_points_types_and_escapes() {
        let m = parse_mountinfo(SAMPLE);
        assert_eq!(m.len(), 3);
        assert_eq!(m[2].point, Path::new("/run/media/me/My Disk"));
        assert_eq!(m[1].fs_type, "btrfs");
        assert_eq!(m[2].source, "/dev/sdb1");
    }

    #[test]
    fn finds_the_longest_matching_mount() {
        let m = parse_mountinfo(SAMPLE);
        assert_eq!(mount_point_of(&m, Path::new("/home/me/x")).unwrap(), Path::new("/home"));
        assert_eq!(mount_point_of(&m, Path::new("/homework")).unwrap(), Path::new("/"));
    }
}
