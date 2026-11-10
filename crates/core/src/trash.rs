//! The freedesktop Trash, spec version 1.0.
//!
//! Files on the same disk as the home Trash go there. Anything else goes to
//! `$topdir/.Trash/$uid` if the admin made a valid `.Trash` (a real folder, sticky),
//! or else `$topdir/.Trash-$uid`. Each item writes its `.trashinfo` first, with
//! `O_EXCL`, and the name reserved that way is then used in `files/`.
//!
//! Nothing here forces an item to disk, because one sync per item made big batches take
//! minutes. The caller syncs each Trash folder once, after the batch. A power cut before
//! that can lose an info file or leave it empty or partial, which leaves the item in
//! `files/` but not restorable.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::mounts::{Mount, mount_point_of, read_mounts};
use crate::ops::rename_noreplace;
use crate::time;
use crate::uri::{decode_path, encode_path};

/// `path -> mount point of the disk holding it`.
pub type MountLookup = Arc<dyn Fn(&Path) -> Option<PathBuf> + Send + Sync>;

#[derive(Clone)]
pub struct TrashConfig {
    /// `$XDG_DATA_HOME/Trash`, or `~/.local/share/Trash`.
    pub home_trash: PathBuf,
    pub uid: u32,
    pub mount_of: MountLookup,
    /// Test seam: treat everything as being on a different disk from the home Trash.
    pub ignore_home_trash: bool,
}

impl TrashConfig {
    pub fn new(data_home: &Path, uid: u32) -> Self {
        Self {
            home_trash: data_home.join("Trash"),
            uid,
            mount_of: Arc::new(|p| mount_point_of(&read_mounts(), p)),
            ignore_home_trash: false,
        }
    }
}

/// An item now in the Trash, with what's needed to restore it.
#[derive(Clone, Debug, PartialEq)]
pub struct Trashed {
    pub original: PathBuf,
    pub file: PathBuf,
    pub info: PathBuf,
}

#[derive(Debug)]
pub enum TrashError {
    /// Nowhere on this disk can hold a Trash. The UI offers to delete for good.
    NoTrashHere(io::Error),
    /// The item is already in the Trash.
    AlreadyTrashed,
    Io(io::Error),
}

impl std::fmt::Display for TrashError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TrashError::NoTrashHere(e) => write!(f, "This disk has no usable Trash ({}). Shift+Delete deletes for good.", crate::listing::describe(e)),
            TrashError::AlreadyTrashed => write!(f, "Already in the Trash. Shift+Delete deletes for good."),
            TrashError::Io(e) => write!(f, "{}", crate::listing::describe(e)),
        }
    }
}

/// One candidate Trash folder, and what `Path=` in its info files is relative to.
struct Place {
    root: PathBuf,
    relative_to: Option<PathBuf>,
    /// On a disk other users can reach, so it has to be verified as ours.
    shared: bool,
}

/// Each folder's mount point, remembered for one batch. Looking one up reads the whole
/// mount table, which is far too slow to do for every item.
#[derive(Default)]
pub struct MountCache(HashMap<PathBuf, Option<PathBuf>>);

impl MountCache {
    fn top(&mut self, cfg: &TrashConfig, folder: &Path) -> Option<PathBuf> {
        if let Some(top) = self.0.get(folder) {
            return top.clone();
        }
        let top = (cfg.mount_of)(folder);
        self.0.insert(folder.to_path_buf(), top.clone());
        top
    }
}

pub fn trash(cfg: &TrashConfig, path: &Path) -> Result<Trashed, TrashError> {
    trash_in_batch(cfg, path, &mut MountCache::default())
}

pub fn trash_in_batch(cfg: &TrashConfig, path: &Path, mounts: &mut MountCache) -> Result<Trashed, TrashError> {
    let abs = absolute_parent(path).map_err(TrashError::Io)?;
    let meta = fs::symlink_metadata(&abs).map_err(TrashError::Io)?;
    let top = abs.parent().and_then(|p| mounts.top(cfg, p));
    if inside_a_trash(cfg, &abs, top.as_deref()) {
        return Err(TrashError::AlreadyTrashed);
    }
    let mut last_err = None;
    let same_disk_as_home = !cfg.ignore_home_trash && ensure_dirs(&cfg.home_trash).is_ok() && fs::metadata(&cfg.home_trash).is_ok_and(|m| m.dev() == meta.dev());
    let mut places = Vec::new();
    if same_disk_as_home {
        places.push(Place { root: cfg.home_trash.clone(), relative_to: None, shared: false });
    }
    if let Some(top) = top
        // The home Trash can still fail with EXDEV across bind mounts, so these follow it.
        && !top_trash_would_be_home(cfg, &top)
    {
        let admin = top.join(".Trash");
        if valid_admin_trash(&admin) {
            places.push(Place { root: admin.join(cfg.uid.to_string()), relative_to: Some(top.clone()), shared: true });
        }
        places.push(Place { root: top.join(format!(".Trash-{}", cfg.uid)), relative_to: Some(top), shared: true });
    }
    for place in places {
        match move_into(&place, &abs) {
            Ok(t) => return Ok(t),
            Err(Failure::Move(e)) if e.raw_os_error() != Some(libc::EXDEV) => return Err(TrashError::Io(e)),
            Err(Failure::Move(e) | Failure::Place(e)) => last_err = Some(e),
        }
    }
    Err(TrashError::NoTrashHere(last_err.unwrap_or_else(|| io::Error::other("no Trash folder found"))))
}

/// Why an attempt failed: this Trash folder is unusable, so try the next one, or the
/// item itself couldn't move, which the next folder wouldn't change.
enum Failure {
    Place(io::Error),
    Move(io::Error),
}

/// A `.Trash-1000` at the top of the disk could be the home Trash only in odd setups;
/// skipping that case keeps an item from being trashed "into itself".
fn top_trash_would_be_home(cfg: &TrashConfig, top: &Path) -> bool {
    cfg.home_trash == top.join(format!(".Trash-{}", cfg.uid))
}

/// Canonicalizes the folder, not the item: a symlink gets trashed, not its target.
fn absolute_parent(path: &Path) -> io::Result<PathBuf> {
    let name = path.file_name().ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    Ok(fs::canonicalize(parent)?.join(name))
}

/// True only for the real Trash folders this item could be in. A folder that merely
/// has a Trash-like name, like `.Trash-old`, doesn't count.
fn inside_a_trash(cfg: &TrashConfig, abs: &Path, top: Option<&Path>) -> bool {
    let home = fs::canonicalize(&cfg.home_trash).unwrap_or_else(|_| cfg.home_trash.clone());
    if abs.starts_with(&home) {
        return true;
    }
    top.is_some_and(|t| abs.starts_with(t.join(format!(".Trash-{}", cfg.uid))) || abs.starts_with(t.join(".Trash").join(cfg.uid.to_string())))
}

/// The spec: `.Trash` must be a real folder with the sticky bit, not a symlink.
fn valid_admin_trash(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.is_dir() && m.mode() & 0o1000 != 0)
}

pub(crate) fn ensure_dirs(root: &Path) -> io::Result<()> {
    let mut b = fs::DirBuilder::new();
    b.recursive(true).mode(0o700);
    b.create(root.join("files"))?;
    b.create(root.join("info"))
}

/// A shared disk's Trash must be a real folder that we own. Otherwise another user
/// could read what's trashed there, or a symlink could send it somewhere else.
fn verify_ours(root: &Path) -> io::Result<()> {
    let m = fs::symlink_metadata(root)?;
    if m.is_dir() && m.uid() == rustix::process::geteuid().as_raw() {
        Ok(())
    } else {
        Err(io::Error::other("the Trash folder on this disk isn't safe to use"))
    }
}

fn move_into(place: &Place, abs: &Path) -> Result<Trashed, Failure> {
    if place.shared
        && fs::symlink_metadata(&place.root).is_ok()
    {
        verify_ours(&place.root).map_err(Failure::Place)?;
    }
    ensure_dirs(&place.root).map_err(Failure::Place)?;
    if place.shared {
        verify_ours(&place.root).map_err(Failure::Place)?;
    }
    let shown = match &place.relative_to {
        Some(top) => abs.strip_prefix(top).unwrap_or(abs),
        None => abs,
    };
    let name = abs.file_name().ok_or_else(|| Failure::Move(io::Error::from(io::ErrorKind::InvalidInput)))?;
    let body = format!("[Trash Info]\nPath={}\nDeletionDate={}\n", encode_path(shown), time::local(time::now()).rfc3339_local());
    let (info, file) = reserve(&place.root, name, body.as_bytes()).map_err(Failure::Place)?;
    match rename_noreplace(abs, &file) {
        Ok(()) => Ok(Trashed { original: abs.to_path_buf(), file, info }),
        Err(e) => {
            let _ = fs::remove_file(&info);
            Err(Failure::Move(e))
        }
    }
}

/// Claims a unique name by creating its `.trashinfo` exclusively.
fn reserve(root: &Path, name: &OsStr, body: &[u8]) -> io::Result<(PathBuf, PathBuf)> {
    let base = shorten(name);
    for n in 1u32.. {
        let candidate = if n == 1 { base.clone() } else { numbered(&base, n) };
        let mut info_name = candidate.clone();
        info_name.push(".trashinfo");
        let info = root.join("info").join(&info_name);
        let file = root.join("files").join(&candidate);
        if fs::symlink_metadata(&file).is_ok() {
            continue;
        }
        match OpenOptions::new().write(true).create_new(true).mode(0o600).open(&info) {
            Ok(mut f) => {
                if let Err(e) = f.write_all(body) {
                    let _ = fs::remove_file(&info);
                    return Err(e);
                }
                return Ok((info, file));
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    unreachable!()
}

/// Leaves room for `.trashinfo` and a counter in a 255-byte file name. The original
/// name is kept in the info file, so nothing is lost.
fn shorten(name: &OsStr) -> OsString {
    const MAX: usize = 255 - ".trashinfo".len() - 8;
    let b = name.as_bytes();
    if b.len() <= MAX {
        return name.to_os_string();
    }
    OsString::from_vec(b[..crate::ops::char_boundary_at_most(b, MAX)].to_vec())
}

fn numbered(base: &OsStr, n: u32) -> OsString {
    let b = base.as_bytes();
    // "name.2.txt" keeps the extension, as most file managers do.
    let dot = b.iter().rposition(|&c| c == b'.').filter(|&i| i > 0);
    let mut out = Vec::with_capacity(b.len() + 4);
    match dot {
        Some(i) => {
            out.extend_from_slice(&b[..i]);
            out.extend_from_slice(format!(".{n}").as_bytes());
            out.extend_from_slice(&b[i..]);
        }
        None => {
            out.extend_from_slice(b);
            out.extend_from_slice(format!(".{n}").as_bytes());
        }
    }
    OsString::from_vec(out)
}

/// The `.trashinfo` file paired with something in a Trash's `files` folder, if the path
/// is in one. Deleting an item for good has to take its info file too, or the name
/// stays taken and the Trash lists a file that isn't there.
pub fn info_file_for(cfg: &TrashConfig, path: &Path) -> Option<PathBuf> {
    let files = path.parent()?;
    if files.file_name()? != "files" {
        return None;
    }
    let root = files.parent()?;
    let name = root.file_name()?.as_bytes();
    let is_trash = root == cfg.home_trash || name.starts_with(b".Trash-") || root.parent().and_then(Path::file_name).is_some_and(|n| n == ".Trash");
    if !is_trash {
        return None;
    }
    let mut info = path.file_name()?.to_os_string();
    info.push(".trashinfo");
    Some(root.join("info").join(info))
}

/// Moves an item back. Refuses to overwrite anything that's since taken its place.
pub fn restore(t: &Trashed) -> io::Result<()> {
    if let Some(parent) = t.original.parent() {
        fs::create_dir_all(parent)?;
    }
    rename_noreplace(&t.file, &t.original)?;
    let _ = fs::remove_file(&t.info);
    Ok(())
}

/// A Trash folder that exists and is ours, and what `Path=` in its info files is relative to.
#[derive(Clone, Debug, PartialEq)]
pub struct Root {
    pub root: PathBuf,
    pub relative_to: Option<PathBuf>,
}

impl Root {
    pub fn files(&self) -> PathBuf {
        self.root.join("files")
    }
}

/// Network shares can hang a stat for minutes, so their Trash folders aren't looked for.
const SKIPPED_FS: [&str; 6] = ["nfs", "nfs4", "cifs", "smb3", "sshfs", "fuse.sshfs"];

/// Every Trash folder holding this user's items: the home one first, then one per disk.
/// Only folders that already exist count, and nothing is created.
pub fn roots(home_trash: &Path, uid: u32, mounts: &[Mount]) -> Vec<Root> {
    let mut out = vec![Root { root: home_trash.to_path_buf(), relative_to: None }];
    let home_canon = fs::canonicalize(home_trash).ok();
    for m in mounts {
        if SKIPPED_FS.contains(&m.fs_type.as_str()) || m.point.starts_with("/proc") || m.point.starts_with("/sys") || m.point.starts_with("/dev") || m.point.starts_with("/run/user") {
            continue;
        }
        let admin = m.point.join(".Trash");
        let mut candidates = Vec::new();
        if valid_admin_trash(&admin) {
            candidates.push(admin.join(uid.to_string()));
        }
        candidates.push(m.point.join(format!(".Trash-{uid}")));
        for root in candidates {
            let is_home = root == home_trash || home_canon.as_ref().is_some_and(|h| fs::canonicalize(&root).is_ok_and(|r| &r == h));
            if is_home || out.iter().any(|r| r.root == root) {
                continue;
            }
            if verify_ours(&root).is_ok() && root.join("files").is_dir() {
                out.push(Root { root, relative_to: Some(m.point.clone()) });
            }
        }
    }
    out
}

/// What a `.trashinfo` file says: where the item came from, and when it was trashed.
#[derive(Clone, Debug, PartialEq)]
pub struct Info {
    pub original: PathBuf,
    /// Seconds since the epoch, if the date could be read.
    pub deleted: Option<i64>,
}

/// Reads a `.trashinfo` file's text. A relative `Path=` is relative to the disk's top.
pub fn parse_info(text: &[u8], relative_to: Option<&Path>) -> Option<Info> {
    let text = std::str::from_utf8(text).ok()?;
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
    if lines.next()? != "[Trash Info]" {
        return None;
    }
    let (mut path, mut date) = (None, None);
    for line in lines {
        if line.starts_with('[') {
            break;
        }
        if let Some(v) = line.strip_prefix("Path=") {
            path.get_or_insert(v);
        } else if let Some(v) = line.strip_prefix("DeletionDate=") {
            date.get_or_insert(v);
        }
    }
    let decoded = decode_path(path?);
    let original = if decoded.is_absolute() {
        decoded
    } else {
        relative_to?.join(decoded)
    };
    // `..` could aim a restore anywhere, so such a record isn't believed.
    if original.components().any(|c| matches!(c, std::path::Component::ParentDir)) || original.file_name().is_none() {
        return None;
    }
    let deleted = date.and_then(time::Local::parse_rfc3339_local).and_then(time::from_local);
    Some(Info { original, deleted })
}

/// The info file that goes with `name` in `files/`.
pub fn info_path(root: &Path, name: &OsStr) -> PathBuf {
    let mut info = name.to_os_string();
    info.push(".trashinfo");
    root.join("info").join(info)
}

/// True for anything inside one of this user's Trash `files` folders, at any depth.
pub fn is_inside_trash(home_trash: &Path, uid: u32, path: &Path) -> bool {
    if path.starts_with(home_trash.join("files")) {
        return true;
    }
    let uid = uid.to_string();
    let parts: Vec<&OsStr> = path.iter().collect();
    parts.windows(2).enumerate().any(|(i, w)| {
        w[1] == "files" && (w[0].as_bytes() == format!(".Trash-{uid}").as_bytes() || (w[0] == uid.as_str() && i > 0 && parts[i - 1] == ".Trash"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::Scratch;
    use std::os::unix::fs::{PermissionsExt, symlink};

    fn cfg(s: &Scratch) -> TrashConfig {
        let mut c = TrashConfig::new(&s.path().join("data"), 1000);
        // Tests must never see the real mount table, or a test could reach a real disk's Trash.
        c.mount_of = Arc::new(|_| None);
        c
    }

    fn info_lines(t: &Trashed) -> Vec<String> {
        fs::read_to_string(&t.info).unwrap().lines().map(str::to_string).collect()
    }

    #[test]
    fn moves_to_the_home_trash_with_a_valid_info_file() {
        let s = Scratch::new();
        let f = s.file("work/a b.txt", b"hello");
        let t = trash(&cfg(&s), &f).unwrap();
        assert!(!f.exists());
        assert_eq!(fs::read(&t.file).unwrap(), b"hello");
        assert_eq!(t.file, s.path().join("data/Trash/files/a b.txt"));
        assert_eq!(t.info, s.path().join("data/Trash/info/a b.txt.trashinfo"));
        let lines = info_lines(&t);
        assert_eq!(lines[0], "[Trash Info]");
        assert_eq!(lines[1], format!("Path={}", encode_path(&fs::canonicalize(s.path()).unwrap().join("work/a b.txt"))));
        assert!(lines[1].contains("a%20b.txt"));
        let date = lines[2].strip_prefix("DeletionDate=").unwrap();
        assert_eq!(date.len(), 19);
        assert_eq!(&date[10..11], "T");
    }

    #[test]
    fn same_name_twice_gets_a_new_name() {
        let s = Scratch::new();
        let c = cfg(&s);
        let t1 = trash(&c, &s.file("work/a.txt", b"1")).unwrap();
        let t2 = trash(&c, &s.file("work/a.txt", b"2")).unwrap();
        let t3 = trash(&c, &s.file("work/a.txt", b"3")).unwrap();
        assert_eq!(t1.file.file_name().unwrap(), "a.txt");
        assert_eq!(t2.file.file_name().unwrap(), "a.2.txt");
        assert_eq!(t3.file.file_name().unwrap(), "a.3.txt");
        assert_eq!(fs::read(&t2.file).unwrap(), b"2");
    }

    #[test]
    fn folders_and_symlinks_keep_their_shape() {
        let s = Scratch::new();
        let c = cfg(&s);
        s.file("work/d/inner", b"x");
        let t = trash(&c, &s.path().join("work/d")).unwrap();
        assert_eq!(fs::read(t.file.join("inner")).unwrap(), b"x");
        let target = s.file("work/real", b"keep");
        let link = s.path().join("work/lnk");
        symlink(&target, &link).unwrap();
        let t = trash(&c, &link).unwrap();
        assert!(fs::symlink_metadata(&t.file).unwrap().file_type().is_symlink());
        assert_eq!(fs::read(&target).unwrap(), b"keep");
    }

    #[test]
    fn restore_puts_it_back_and_refuses_to_overwrite() {
        let s = Scratch::new();
        let c = cfg(&s);
        let f = s.file("work/a.txt", b"old");
        let t = trash(&c, &f).unwrap();
        restore(&t).unwrap();
        assert_eq!(fs::read(&f).unwrap(), b"old");
        assert!(!t.info.exists());
        let t = trash(&c, &f).unwrap();
        fs::write(&f, b"new").unwrap();
        assert!(restore(&t).is_err());
        assert_eq!(fs::read(&f).unwrap(), b"new");
        assert_eq!(fs::read(&t.file).unwrap(), b"old");
    }

    #[test]
    fn restore_recreates_a_missing_parent() {
        let s = Scratch::new();
        let c = cfg(&s);
        let f = s.file("work/sub/a.txt", b"x");
        let t = trash(&c, &f).unwrap();
        fs::remove_dir(s.path().join("work/sub")).unwrap();
        restore(&t).unwrap();
        assert!(f.exists());
    }

    #[test]
    fn items_already_in_the_trash_are_refused() {
        let s = Scratch::new();
        let c = cfg(&s);
        let t = trash(&c, &s.file("work/a", b"")).unwrap();
        assert!(matches!(trash(&c, &t.file), Err(TrashError::AlreadyTrashed)));
    }

    #[test]
    fn a_failed_move_leaves_no_info_file() {
        let s = Scratch::new();
        let c = cfg(&s);
        let d = s.dir("ro");
        let f = s.file("ro/a", b"x");
        fs::set_permissions(&d, fs::Permissions::from_mode(0o500)).unwrap();
        let r = trash(&c, &f);
        fs::set_permissions(&d, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(r.is_err(), "rename out of a read-only folder must fail");
        assert!(f.exists());
        assert_eq!(fs::read_dir(s.path().join("data/Trash/info")).unwrap().count(), 0);
    }

    fn other_disk(s: &Scratch) -> (TrashConfig, PathBuf) {
        let top = s.dir("disk");
        let mut c = cfg(s);
        c.ignore_home_trash = true;
        let canon = fs::canonicalize(&top).unwrap();
        let t = canon.clone();
        c.mount_of = Arc::new(move |_| Some(t.clone()));
        (c, canon)
    }

    #[test]
    fn other_disks_use_the_uid_folder_with_a_relative_path() {
        let s = Scratch::new();
        let (c, top) = other_disk(&s);
        let f = s.file("disk/sub/a.txt", b"x");
        let t = trash(&c, &f).unwrap();
        assert_eq!(t.file, top.join(".Trash-1000/files/a.txt"));
        assert_eq!(info_lines(&t)[1], "Path=sub/a.txt");
        assert_eq!(fs::symlink_metadata(top.join(".Trash-1000")).unwrap().mode() & 0o777, 0o700);
    }

    #[test]
    fn a_valid_admin_trash_is_preferred() {
        let s = Scratch::new();
        let (c, top) = other_disk(&s);
        let admin = top.join(".Trash");
        fs::create_dir(&admin).unwrap();
        fs::set_permissions(&admin, fs::Permissions::from_mode(0o1777)).unwrap();
        let t = trash(&c, &s.file("disk/a", b"x")).unwrap();
        assert_eq!(t.file, top.join(".Trash/1000/files/a"));
    }

    #[test]
    fn an_admin_trash_without_the_sticky_bit_is_skipped() {
        let s = Scratch::new();
        let (c, top) = other_disk(&s);
        fs::create_dir(top.join(".Trash")).unwrap();
        let t = trash(&c, &s.file("disk/a", b"x")).unwrap();
        assert_eq!(t.file, top.join(".Trash-1000/files/a"));
    }

    #[test]
    fn a_symlinked_admin_trash_is_skipped() {
        let s = Scratch::new();
        let (c, top) = other_disk(&s);
        let real = s.dir("elsewhere");
        fs::set_permissions(&real, fs::Permissions::from_mode(0o1777)).unwrap();
        symlink(&real, top.join(".Trash")).unwrap();
        let t = trash(&c, &s.file("disk/a", b"x")).unwrap();
        assert_eq!(t.file, top.join(".Trash-1000/files/a"));
        assert_eq!(fs::read_dir(&real).unwrap().count(), 0);
    }

    #[test]
    fn nowhere_to_trash_is_reported_and_keeps_the_file() {
        let s = Scratch::new();
        let mut c = cfg(&s);
        c.ignore_home_trash = true;
        let f = s.file("a", b"x");
        assert!(matches!(trash(&c, &f), Err(TrashError::NoTrashHere(_))));
        assert!(f.exists());
    }

    #[test]
    fn a_read_only_folder_isnt_blamed_on_the_trash() {
        let s = Scratch::new();
        let c = cfg(&s);
        let d = s.dir("ro");
        let f = s.file("ro/a", b"x");
        fs::set_permissions(&d, fs::Permissions::from_mode(0o500)).unwrap();
        let r = trash(&c, &f);
        fs::set_permissions(&d, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(matches!(r, Err(TrashError::Io(_))), "must not suggest deleting for good");
    }

    #[test]
    fn a_folder_with_a_trash_like_name_is_ordinary() {
        let s = Scratch::new();
        let c = cfg(&s);
        let f = s.file("work/.Trash-old/notes.txt", b"x");
        let t = trash(&c, &f).unwrap();
        assert!(t.file.exists());
    }

    #[test]
    fn a_symlinked_data_home_still_knows_whats_trashed() {
        let s = Scratch::new();
        let real = s.dir("realdata");
        symlink(&real, s.path().join("linkdata")).unwrap();
        let mut c = TrashConfig::new(&s.path().join("linkdata"), 1000);
        c.mount_of = Arc::new(|_| None);
        let t = trash(&c, &s.file("work/a", b"x")).unwrap();
        let in_trash = fs::canonicalize(&t.file).unwrap();
        assert!(matches!(trash(&c, &in_trash), Err(TrashError::AlreadyTrashed)));
    }

    #[test]
    fn a_symlinked_uid_trash_isnt_used() {
        let s = Scratch::new();
        let (c, top) = other_disk(&s);
        let elsewhere = s.dir("attacker");
        symlink(&elsewhere, top.join(".Trash-1000")).unwrap();
        let f = s.file("disk/a", b"x");
        assert!(matches!(trash(&c, &f), Err(TrashError::NoTrashHere(_))));
        assert!(f.exists());
        assert_eq!(fs::read_dir(&elsewhere).unwrap().count(), 0);
    }

    #[test]
    fn long_trash_names_dont_cut_a_character_in_half() {
        let s = Scratch::new();
        let c = cfg(&s);
        let f = s.file(&format!("work/{}", "é".repeat(127)), b"x");
        let t = trash(&c, &f).unwrap();
        assert!(t.file.file_name().unwrap().to_str().is_some());
    }

    #[test]
    fn info_files_are_found_only_for_real_trash_folders() {
        let s = Scratch::new();
        let c = cfg(&s);
        let t = trash(&c, &s.file("work/a", b"")).unwrap();
        assert_eq!(info_file_for(&c, &t.file), Some(t.info.clone()));
        assert_eq!(info_file_for(&c, Path::new("/m/.Trash-1000/files/x")), Some("/m/.Trash-1000/info/x.trashinfo".into()));
        assert_eq!(info_file_for(&c, Path::new("/m/.Trash/1000/files/x")), Some("/m/.Trash/1000/info/x.trashinfo".into()));
        assert_eq!(info_file_for(&c, Path::new("/home/me/files/x")), None);
    }

    #[test]
    fn very_long_names_still_fit() {
        let s = Scratch::new();
        let c = cfg(&s);
        let long = "n".repeat(255);
        let f = s.file(&format!("work/{long}"), b"x");
        let t = trash(&c, &f).unwrap();
        assert!(t.file.exists() && t.info.exists());
        assert!(info_lines(&t)[1].contains(&long));
    }

    #[test]
    fn non_utf8_names_are_encoded() {
        let s = Scratch::new();
        let c = cfg(&s);
        let d = s.dir("work");
        let f = d.join(OsStr::from_bytes(b"bad\xff"));
        fs::write(&f, b"x").unwrap();
        let t = trash(&c, &f).unwrap();
        assert!(info_lines(&t)[1].ends_with("bad%FF"));
    }

    fn mount(point: &Path, fs_type: &str) -> Mount {
        Mount { point: point.to_path_buf(), fs_type: fs_type.into(), source: "/dev/sdz1".into() }
    }

    fn me() -> u32 {
        rustix::process::geteuid().as_raw()
    }

    #[test]
    fn info_files_are_read_with_paths_made_whole() {
        let abs = parse_info(b"[Trash Info]\nPath=/home/me/a%20b.txt\nDeletionDate=2026-11-10T08:30:00\n", None).unwrap();
        assert_eq!(abs.original, Path::new("/home/me/a b.txt"));
        assert_eq!(abs.deleted, time::Local::parse_rfc3339_local("2026-11-10T08:30:00").and_then(time::from_local));
        let rel = parse_info(b"[Trash Info]\nPath=sub/x\n", Some(Path::new("/mnt/usb"))).unwrap();
        assert_eq!(rel.original, Path::new("/mnt/usb/sub/x"));
        assert_eq!(rel.deleted, None);
    }

    #[test]
    fn info_files_that_cant_be_trusted_are_refused() {
        assert_eq!(parse_info(b"Path=/a\n", None), None, "no header");
        assert_eq!(parse_info(b"[Trash Info]\nDeletionDate=2026-11-10T08:30:00\n", None), None, "no path");
        assert_eq!(parse_info(b"[Trash Info]\nPath=x\n", None), None, "relative, with nothing to be relative to");
        assert_eq!(parse_info(b"[Trash Info]\nPath=/home/me/../../etc/passwd\n", None), None, "climbs out");
        assert_eq!(parse_info(b"[Trash Info]\nPath=/\n", None), None, "no name");
        assert_eq!(parse_info(b"[Other]\n[Trash Info]\nPath=/a\n", None), None, "header not first");
    }

    #[test]
    fn roots_are_home_first_then_each_disk_with_a_trash_of_ours() {
        let s = Scratch::new();
        let home = s.path().join("data/Trash");
        let disk = s.dir("disk");
        fs::create_dir_all(disk.join(format!(".Trash-{}/files", me()))).unwrap();
        let admin = s.dir("admin");
        fs::create_dir(admin.join(".Trash")).unwrap();
        fs::set_permissions(admin.join(".Trash"), fs::Permissions::from_mode(0o1777)).unwrap();
        fs::create_dir_all(admin.join(format!(".Trash/{}/files", me()))).unwrap();
        let bare = s.dir("bare");
        let net = s.dir("net");
        fs::create_dir_all(net.join(format!(".Trash-{}/files", me()))).unwrap();
        let mounts = vec![mount(&disk, "ext4"), mount(&admin, "vfat"), mount(&bare, "ext4"), mount(&net, "nfs4"), mount(&disk, "ext4")];
        let got = roots(&home, me(), &mounts);
        assert_eq!(got.len(), 3, "{got:?}");
        assert_eq!(got[0], Root { root: home.clone(), relative_to: None });
        assert_eq!(got[1], Root { root: disk.join(format!(".Trash-{}", me())), relative_to: Some(disk.clone()) });
        assert_eq!(got[2], Root { root: admin.join(format!(".Trash/{}", me())), relative_to: Some(admin.clone()) });
    }

    #[test]
    fn a_disk_trash_that_isnt_ours_or_is_a_symlink_is_left_out() {
        let s = Scratch::new();
        let disk = s.dir("disk");
        let elsewhere = s.dir("elsewhere");
        fs::create_dir(elsewhere.join("files")).unwrap();
        symlink(&elsewhere, disk.join(format!(".Trash-{}", me()))).unwrap();
        let got = roots(&s.path().join("data/Trash"), me(), &[mount(&disk, "ext4")]);
        assert_eq!(got.len(), 1);
    }

    #[test]
    fn inside_trash_means_inside_a_files_folder_of_ours() {
        let home = Path::new("/h/.local/share/Trash");
        assert!(is_inside_trash(home, 1000, Path::new("/h/.local/share/Trash/files")));
        assert!(is_inside_trash(home, 1000, Path::new("/h/.local/share/Trash/files/d/e")));
        assert!(is_inside_trash(home, 1000, Path::new("/mnt/u/.Trash-1000/files/d")));
        assert!(is_inside_trash(home, 1000, Path::new("/mnt/u/.Trash/1000/files")));
        assert!(!is_inside_trash(home, 1000, Path::new("/mnt/u/.Trash-1001/files")));
        assert!(!is_inside_trash(home, 1000, Path::new("/mnt/u/1000/files")));
        assert!(!is_inside_trash(home, 1000, Path::new("/h/.local/share/Trash/info")));
        assert!(!is_inside_trash(home, 1000, Path::new("/h/work/files")));
    }
}
