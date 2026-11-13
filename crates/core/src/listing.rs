//! Folder listings, built on worker threads and handed to the UI whole.
//!
//! A listing is immutable once published, except that sizes and times start unknown
//! and fill in through atomics as they're needed. Only visible rows get a stat unless
//! a sort needs every row, so big folders on slow disks stay cheap. A hung disk can
//! hang only its own worker thread: nothing waits for one.

use std::cmp::Ordering;
use std::ffi::{CStr, CString, OsStr, OsString};
use std::fs::{File, OpenOptions};
use std::mem::MaybeUninit;
use std::os::fd::AsFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{self, AtomicBool, AtomicI64, AtomicU64, AtomicUsize};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use rustix::fs::{AtFlags, FileType, RawDir, StatxFlags, statat, statx};

use crate::mounts::read_mounts;
use crate::sort::{natural_cmp, par_sort_by};
use crate::trash::{self, Info, Root, Trashed};
use crate::watch::Watch;

const SIZE_UNKNOWN: u64 = u64::MAX;
const SIZE_FAILED: u64 = u64::MAX - 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    /// A symlink to a file, or a broken one.
    Link,
    /// A symlink to a folder: it behaves as a folder for sorting and opening.
    LinkDir,
    Other,
}

impl Kind {
    pub fn is_dirlike(self) -> bool {
        matches!(self, Kind::Dir | Kind::LinkDir)
    }
}

#[derive(Clone, Copy)]
struct Slot {
    off: u32,
    len: u16,
    kind: Kind,
}

struct Meta {
    size: AtomicU64,
    mtime: AtomicI64,
}

/// What a row's size and modified columns show.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Stat {
    Unknown,
    Failed,
    Known { size: u64, mtime: i64 },
}

pub struct Listing {
    pub dir: PathBuf,
    /// Stays open so stats are relative to this folder even if it gets renamed.
    fd: Option<Arc<File>>,
    names: Vec<u8>,
    slots: Vec<Slot>,
    meta: Vec<Meta>,
    pub complete: bool,
    /// Set if reading stopped early; the entries are what was read before that.
    pub read_error: Option<String>,
    slow_stat: Option<Duration>,
    /// Set for the Trash view, whose rows come from several Trash folders.
    trash: Option<Arc<TrashRows>>,
    /// Set for the Recent view, whose rows are files anywhere.
    recent: Option<Arc<Vec<PathBuf>>>,
}

#[derive(Clone)]
/// Where each row of the Trash view really is, and what its info file says.
/// Rows are named after the item's original name, which can differ from its name in
/// `files/` when several items with one name were trashed.
pub struct TrashRows {
    pub roots: Vec<Root>,
    fds: Vec<Arc<File>>,
    root_of: Vec<u16>,
    file_names: Vec<OsString>,
    info: Vec<Option<Info>>,
}

impl Listing {
    /// Every folder this listing holds open, both as it was named and where it really is, with
    /// symlinks resolved: its own, or for the Trash view, each Trash folder it reads from, which
    /// can be on another drive. Where one really is comes from the kernel's record of the open
    /// folder, so this doesn't touch the filesystem, and can't hang on one that's stopped
    /// answering.
    pub fn held_dirs(&self) -> Vec<PathBuf> {
        use std::os::fd::AsRawFd;
        let real = |f: &Arc<File>| std::fs::read_link(format!("/proc/self/fd/{}", f.as_raw_fd())).ok();
        let mut out = vec![self.dir.clone()];
        out.extend(self.fd.iter().filter_map(real));
        if let Some(t) = &self.trash {
            out.extend(t.roots.iter().map(|r| r.root.clone()));
            out.extend(t.fds.iter().filter_map(real));
        }
        out
    }

    pub fn len(&self) -> usize {
        self.slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    pub fn name(&self, i: u32) -> &[u8] {
        let s = self.slots[i as usize];
        &self.names[s.off as usize..s.off as usize + s.len as usize]
    }

    pub fn os_name(&self, i: u32) -> &OsStr {
        OsStr::from_bytes(self.name(i))
    }

    pub fn path(&self, i: u32) -> PathBuf {
        match &self.trash {
            Some(t) => t.roots[t.root_of[i as usize] as usize].files().join(&t.file_names[i as usize]),
            None => match &self.recent {
                Some(paths) => paths[i as usize].clone(),
                None => self.dir.join(self.os_name(i)),
            },
        }
    }

    /// What tells this row apart from the others, for finding it again after a reload.
    /// It's the name, except in the Trash and Recent views, where two rows can share a name.
    pub fn key(&self, i: u32) -> std::borrow::Cow<'_, [u8]> {
        if self.trash.is_some() || self.recent.is_some() {
            std::borrow::Cow::Owned(self.path(i).into_os_string().into_vec())
        } else {
            std::borrow::Cow::Borrowed(self.name(i))
        }
    }

    /// True for the Recent view.
    pub fn is_recent(&self) -> bool {
        self.recent.is_some()
    }

    /// The folder a row is in, for the views whose rows come from many: where a Trash item
    /// was deleted from, if its info file could be read, or where a recent file is.
    pub fn location(&self, i: u32) -> Option<&Path> {
        match &self.recent {
            Some(paths) => paths[i as usize].parent(),
            None => self.original(i).and_then(Path::parent),
        }
    }

    /// True for the Trash view.
    pub fn is_trash(&self) -> bool {
        self.trash.is_some()
    }

    /// Where a row of the Trash view came from, if its info file could be read.
    pub fn original(&self, i: u32) -> Option<&Path> {
        self.trash.as_ref()?.info[i as usize].as_ref().map(|inf| inf.original.as_path())
    }

    /// What restoring a row of the Trash view needs, if its info file could be read.
    pub fn trashed(&self, i: u32) -> Option<Trashed> {
        let t = self.trash.as_ref()?;
        let info = t.info[i as usize].as_ref()?;
        let root = &t.roots[t.root_of[i as usize] as usize];
        let name = &t.file_names[i as usize];
        Some(Trashed { original: info.original.clone(), file: root.files().join(name), info: trash::info_path(&root.root, name) })
    }

    pub fn kind(&self, i: u32) -> Kind {
        self.slots[i as usize].kind
    }

    pub fn hidden(&self, i: u32) -> bool {
        self.name(i).first() == Some(&b'.')
    }

    pub fn stat(&self, i: u32) -> Stat {
        let m = &self.meta[i as usize];
        match m.size.load(atomic::Ordering::Acquire) {
            SIZE_UNKNOWN => Stat::Unknown,
            SIZE_FAILED => Stat::Failed,
            size => Stat::Known { size, mtime: m.mtime.load(atomic::Ordering::Relaxed) },
        }
    }

    pub fn find(&self, name: &[u8]) -> Option<u32> {
        (0..self.len() as u32).find(|&i| self.name(i) == name)
    }

    fn stat_one(&self, i: u32) {
        let m = &self.meta[i as usize];
        if m.size.load(atomic::Ordering::Relaxed) != SIZE_UNKNOWN {
            return;
        }
        if let Some(d) = self.slow_stat {
            std::thread::sleep(d);
        }
        if self.recent.is_some() {
            // Each recent file is stat'ed as it's read, which also gives its date of use.
            m.size.store(SIZE_FAILED, atomic::Ordering::Relaxed);
            return;
        }
        let (fd, name) = match &self.trash {
            Some(t) => (Some(&t.fds[t.root_of[i as usize] as usize]), t.file_names[i as usize].as_bytes()),
            None => (self.fd.as_ref(), self.name(i)),
        };
        let Some(fd) = fd else {
            m.size.store(SIZE_FAILED, atomic::Ordering::Relaxed);
            return;
        };
        // In the Trash, the date column is when the item was trashed.
        let deleted = self.trash.as_ref().and_then(|t| t.info[i as usize].as_ref()).and_then(|inf| inf.deleted);
        let flags = AtFlags::SYMLINK_NOFOLLOW | AtFlags::NO_AUTOMOUNT;
        let got = std::ffi::CString::new(name).ok().and_then(|n| statx(fd.as_fd(), n.as_c_str(), flags, StatxFlags::SIZE | StatxFlags::MTIME).ok());
        match got {
            Some(st) => {
                // Release pairs with the Acquire in stat(): a reader that sees the size sees the time.
                m.mtime.store(deleted.unwrap_or(st.stx_mtime.tv_sec), atomic::Ordering::Relaxed);
                m.size.store(st.stx_size, atomic::Ordering::Release);
            }
            None => m.size.store(SIZE_FAILED, atomic::Ordering::Relaxed),
        }
    }

    /// Stats the given entries; true if any of them still needed it.
    pub fn stat_entries(&self, ids: &[u32]) -> bool {
        let mut any = false;
        for &i in ids {
            if self.stat(i) == Stat::Unknown {
                self.stat_one(i);
                any = true;
            }
        }
        any
    }

    /// Stats every entry on several threads. Stops early if `cancel` gets set.
    pub fn stat_all(&self, cancel: &AtomicBool) {
        let next = AtomicUsize::new(0);
        let threads = std::thread::available_parallelism().map_or(1, |n| n.get()).min(4);
        std::thread::scope(|s| {
            for _ in 0..threads {
                s.spawn(|| {
                    while !cancel.load(atomic::Ordering::Relaxed) {
                        let start = next.fetch_add(512, atomic::Ordering::Relaxed);
                        if start >= self.len() {
                            return;
                        }
                        for i in start..(start + 512).min(self.len()) {
                            self.stat_one(i as u32);
                        }
                    }
                });
            }
        });
    }

    fn snapshot(&self) -> Listing {
        Listing {
            dir: self.dir.clone(),
            fd: self.fd.clone(),
            names: self.names.clone(),
            slots: self.slots.clone(),
            meta: self.slots.iter().map(|_| new_meta()).collect(),
            complete: false,
            read_error: None,
            slow_stat: self.slow_stat,
            trash: self.trash.clone(),
            recent: self.recent.clone(),
        }
    }
}

fn new_meta() -> Meta {
    Meta { size: AtomicU64::new(SIZE_UNKNOWN), mtime: AtomicI64::new(0) }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortKey {
    Name,
    Size,
    Modified,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SortSpec {
    pub key: SortKey,
    pub descending: bool,
}

impl Default for SortSpec {
    fn default() -> Self {
        Self { key: SortKey::Name, descending: false }
    }
}

/// Sorts every entry: folders first, then by the key, with the name breaking ties.
/// Sorting by size or time stats the whole folder first, so call this off the UI thread.
pub fn sort_entries(l: &Listing, spec: SortSpec, cancel: &AtomicBool) -> Vec<u32> {
    if spec.key != SortKey::Name {
        l.stat_all(cancel);
    }
    sort_known(l, spec)
}

/// Sorts using whatever stats are known right now, without fetching any.
fn sort_known(l: &Listing, spec: SortSpec) -> Vec<u32> {
    // Stats can change while we sort, as other threads fill them in, and a comparator
    // whose answers change breaks the sort. So the keys are copied first.
    let keys: Vec<i128> = match spec.key {
        SortKey::Name => Vec::new(),
        SortKey::Size => (0..l.len() as u32).map(|i| size_of(l, i) as i128).collect(),
        SortKey::Modified => (0..l.len() as u32).map(|i| time_of(l, i) as i128).collect(),
    };
    let mut v: Vec<u32> = (0..l.len() as u32).collect();
    par_sort_by(&mut v, |&a, &b| {
        // Recent is in order of use, folders among files.
        let (da, db) = (l.kind(a).is_dirlike() && !l.is_recent(), l.kind(b).is_dirlike() && !l.is_recent());
        db.cmp(&da).then_with(|| {
            let by_key = match spec.key {
                SortKey::Name => natural_cmp(l.name(a), l.name(b)),
                // Folder sizes mean nothing, so folders stay in name order.
                SortKey::Size if da => Ordering::Equal,
                _ => keys[a as usize].cmp(&keys[b as usize]),
            };
            let by_key = if spec.descending { by_key.reverse() } else { by_key };
            by_key.then_with(|| natural_cmp(l.name(a), l.name(b)))
        })
    });
    v
}

fn size_of(l: &Listing, i: u32) -> u64 {
    match l.stat(i) {
        Stat::Known { size, .. } => size,
        _ => 0,
    }
}

fn time_of(l: &Listing, i: u32) -> i64 {
    match l.stat(i) {
        Stat::Known { mtime, .. } => mtime,
        _ => 0,
    }
}

/// Picks the rows to show from a sorted list.
pub fn visible_rows(l: &Listing, sorted: &[u32], show_hidden: bool, filter: &str) -> Vec<u32> {
    let needle = filter.to_lowercase();
    sorted
        .iter()
        .copied()
        .filter(|&i| (show_hidden || !l.hidden(i)) && (needle.is_empty() || name_contains(l.name(i), &needle)))
        .collect()
}

/// Case-insensitive substring match. `needle` must be lowercase.
pub fn name_contains(name: &[u8], needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    if name.is_ascii() && needle.is_ascii() {
        let n = needle.as_bytes();
        n.len() <= name.len() && name.windows(n.len()).any(|w| w.iter().zip(n).all(|(a, b)| a.to_ascii_lowercase() == *b))
    } else {
        String::from_utf8_lossy(name).to_lowercase().contains(needle)
    }
}

#[derive(Clone, Debug)]
pub struct LoadRequest {
    pub dir: PathBuf,
    pub generation: u64,
    pub sort: SortSpec,
    pub show_hidden: bool,
    pub filter: String,
    /// Which part of the result the window will show first, by row. Stats for these
    /// get a brief head start so rows don't appear with blank columns, but a stat that
    /// stalls never holds the rows back.
    pub first_row: usize,
    pub row_count: usize,
    /// Bench and test seam: sleep this long after each read, to imitate a slow disk.
    pub slow_read: Option<Duration>,
    /// Test seam: sleep this long before each stat, to imitate a dead mount under the folder.
    pub slow_stat: Option<Duration>,
    /// List every Trash folder instead of `dir`, which should be the home Trash's `files`.
    pub trash: Option<TrashSpec>,
    /// List the recently used files instead, from the list at `dir`.
    pub recent: bool,
}

#[derive(Clone, Debug)]
pub struct TrashSpec {
    /// `$XDG_DATA_HOME/Trash`.
    pub home_trash: PathBuf,
    pub uid: u32,
    /// Test seam: the mount table to look for other disks' Trash folders in.
    pub mounts: Option<Vec<crate::mounts::Mount>>,
    /// How long other disks get before the list finishes without them.
    pub disks_give_up: Duration,
}

/// What `TrashSpec::disks_give_up` normally is.
pub const DISKS_GIVE_UP: Duration = Duration::from_secs(3);

pub struct Loaded {
    pub generation: u64,
    pub listing: Arc<Listing>,
    pub sorted: Vec<u32>,
    pub rows: Vec<u32>,
    pub sort: SortSpec,
    /// Nothing more is coming for this request. An unsettled result can be partial, or
    /// in name order while stats for the requested sort are still arriving.
    pub settled: bool,
}

pub enum LoadEvent {
    /// A snapshot. More follow unless `settled`.
    Ready(Loaded),
    Failed { generation: u64, dir: PathBuf, error: String },
}

pub struct LoadHandle {
    cancel: Arc<AtomicBool>,
}

impl LoadHandle {
    pub fn cancel(&self) {
        self.cancel.store(true, atomic::Ordering::Relaxed);
    }
}

const STAT_HEAD_START: Duration = Duration::from_millis(20);
const RESOLVE_HEAD_START: Duration = Duration::from_millis(100);
const SORT_STAT_HEAD_START: Duration = Duration::from_millis(200);
const READ_BUFFER: usize = 32 * 1024;

type LoadSink = Arc<dyn Fn(LoadEvent) + Send + Sync>;

/// Reads the folder on a new thread. `sink` runs on that thread, except if no thread
/// can be started, when it runs here with the failure. A hung read leaves only that
/// thread stuck; cancelling means its results get ignored.
pub fn spawn_load(req: LoadRequest, sink: impl Fn(LoadEvent) + Send + Sync + 'static) -> LoadHandle {
    spawn_load_watching(req, None, sink)
}

/// As `spawn_load`, also pointing `watch` at the folder first, on the same thread: that
/// can hang just as reading can, and a change after the watch but before the read can't
/// be missed.
pub fn spawn_load_watching(req: LoadRequest, watch: Option<Arc<Watch>>, sink: impl Fn(LoadEvent) + Send + Sync + 'static) -> LoadHandle {
    let cancel = Arc::new(AtomicBool::new(false));
    let c = cancel.clone();
    let sink: LoadSink = Arc::new(sink);
    let (s2, r2) = (sink.clone(), req.clone());
    let spawned = std::thread::Builder::new().name("gannet-list".into()).spawn(move || {
        let (generation, dir) = (req.generation, req.dir.clone());
        let done = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match req.trash.clone() {
            Some(spec) => load_trash(req, &spec, watch.as_ref(), &c, &s2),
            None if req.recent => load_recent(req, watch.as_ref(), &c, &s2),
            None => {
                if let Some(w) = &watch {
                    w.set_dirs_unless(std::slice::from_ref(&dir), || c.load(atomic::Ordering::Relaxed));
                }
                load(req, &c, &s2)
            }
        }));
        if done.is_err() {
            s2(LoadEvent::Failed { generation, dir, error: "Reading this folder failed unexpectedly".into() });
        }
    });
    if let Err(e) = spawned {
        sink(LoadEvent::Failed { generation: r2.generation, dir: r2.dir, error: describe(&e) });
    }
    LoadHandle { cancel }
}

fn open_dir(path: &std::path::Path) -> std::io::Result<File> {
    // O_DIRECTORY fails at once on anything else; opening a FIFO without it would hang.
    OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY | libc::O_NONBLOCK).open(path)
}

fn load(req: LoadRequest, cancel: &Arc<AtomicBool>, sink: &LoadSink) {
    let file = match open_dir(&req.dir) {
        Ok(f) => Arc::new(f),
        Err(e) => {
            sink(LoadEvent::Failed { generation: req.generation, dir: req.dir.clone(), error: describe(&e) });
            return;
        }
    };
    let mut l = Listing { dir: req.dir.clone(), fd: Some(file.clone()), names: Vec::new(), slots: Vec::new(), meta: Vec::new(), complete: false, read_error: None, slow_stat: req.slow_stat, trash: None, recent: None };
    // Small enough that a slow disk returns often, and big enough to be cheap on a fast one.
    let mut buf: Vec<MaybeUninit<u8>> = vec![MaybeUninit::uninit(); if req.slow_read.is_some() { 4096 } else { READ_BUFFER }];
    let mut iter = RawDir::new(file.as_fd(), &mut buf);
    let started = Instant::now();
    let mut next_publish = Duration::from_millis(50);
    // Symlinks and unknown types need a stat, which can hang. They wait until after
    // reading, so one link into a dead mount can't stop the folder from showing.
    let mut unresolved: Vec<u32> = Vec::new();
    while let Some(e) = iter.next() {
        let e = match e {
            Ok(e) => e,
            Err(err) => {
                // Show what we managed to read, and say that it's incomplete.
                l.read_error = Some(describe(&err.into()));
                break;
            }
        };
        let name = e.file_name();
        let b = name.to_bytes();
        if b == b"." || b == b".." {
            continue;
        }
        if b.len() > u16::MAX as usize || l.names.len() + b.len() > u32::MAX as usize {
            continue;
        }
        let kind = match e.file_type() {
            FileType::RegularFile => Kind::File,
            FileType::Directory => Kind::Dir,
            FileType::Symlink => {
                unresolved.push(l.slots.len() as u32);
                Kind::Link
            }
            FileType::Unknown => {
                unresolved.push(l.slots.len() as u32);
                Kind::Other
            }
            _ => Kind::Other,
        };
        l.slots.push(Slot { off: l.names.len() as u32, len: b.len() as u16, kind });
        l.names.extend_from_slice(b);
        // The end of a buffer is where the next read, the slow part, would start.
        if iter.is_buffer_empty() {
            if cancel.load(atomic::Ordering::Relaxed) {
                return;
            }
            if let Some(d) = req.slow_read {
                std::thread::sleep(d);
            }
            if started.elapsed() >= next_publish {
                publish_partial(l.snapshot(), &req, cancel, sink);
                next_publish = started.elapsed() * 2;
            }
        }
    }
    if cancel.load(atomic::Ordering::Relaxed) {
        return;
    }
    resolve_kinds(&mut l, unresolved, &req, cancel, sink);
    l.complete = true;
    l.names.shrink_to_fit();
    l.slots.shrink_to_fit();
    l.meta = l.slots.iter().map(|_| new_meta()).collect();
    publish_final(Arc::new(l), &req, cancel, sink);
}

/// What the thread reading other disks' Trash folders sends: the folders to watch, once
/// they're known, then each folder as it's read.
enum DiskMsg {
    Watch(Vec<PathBuf>),
    Root(Root, Arc<File>, Vec<TrashEntry>),
}

/// One row of a Trash folder: its name in `files/`, its kind, and its info file.
type TrashEntry = (OsString, Kind, Option<Info>);

/// Reads one Trash folder whole. Trash folders are small next to the folders `load` is
/// built for, so there's no partial publishing within one.
fn read_trash_root(root: &Root, slow_read: Option<Duration>, cancel: &AtomicBool) -> std::io::Result<(Arc<File>, Vec<TrashEntry>)> {
    let files = root.files();
    let file = Arc::new(open_dir(&files)?);
    let mut out = Vec::new();
    for e in std::fs::read_dir(&files)? {
        if cancel.load(atomic::Ordering::Relaxed) {
            break;
        }
        let Ok(e) = e else { continue };
        let name = e.file_name();
        let kind = match e.file_type() {
            Ok(ft) if ft.is_symlink() => CString::new(name.as_bytes()).map_or(Kind::Other, |n| resolved_kind(&file, &n)),
            Ok(ft) if ft.is_dir() => Kind::Dir,
            Ok(ft) if ft.is_file() => Kind::File,
            _ => Kind::Other,
        };
        let info = read_small(&trash::info_path(&root.root, &name)).and_then(|b| trash::parse_info(&b, root.relative_to.as_deref()));
        out.push((name, kind, info));
        if let Some(d) = slow_read {
            std::thread::sleep(d);
        }
    }
    Ok((file, out))
}

/// Reads an info file, if it's a plain file of sensible size. Opening a FIFO without
/// O_NONBLOCK would hang, and a huge file isn't a real info file.
fn read_small(path: &Path) -> Option<Vec<u8>> {
    use std::io::Read;
    const MAX: u64 = 64 * 1024;
    let f = OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW).open(path).ok()?;
    let m = f.metadata().ok()?;
    if !m.is_file() || m.len() > MAX {
        return None;
    }
    let mut buf = Vec::with_capacity(m.len() as usize);
    f.take(MAX).read_to_end(&mut buf).ok()?;
    Some(buf)
}

fn add_trash_root(l: &mut Listing, t: &mut TrashRows, root: Root, file: Arc<File>, entries: Vec<TrashEntry>) {
    let ri = t.roots.len() as u16;
    for (name, kind, info) in entries {
        let shown = info.as_ref().and_then(|i| i.original.file_name()).unwrap_or(&name).as_bytes();
        if shown.len() > u16::MAX as usize || l.names.len() + shown.len() > u32::MAX as usize {
            continue;
        }
        l.slots.push(Slot { off: l.names.len() as u32, len: shown.len() as u16, kind });
        l.names.extend_from_slice(shown);
        t.root_of.push(ri);
        t.file_names.push(name);
        t.info.push(info);
    }
    t.roots.push(root);
    t.fds.push(file);
}

/// Lists what's in every Trash folder, as one list. The home Trash is read first. Other
/// disks are found and read on a thread of their own: a disk that stops answering can
/// hang that thread, but the home Trash still shows.
fn load_trash(req: LoadRequest, spec: &TrashSpec, watch: Option<&Arc<Watch>>, cancel: &Arc<AtomicBool>, sink: &LoadSink) {
    let stale = || cancel.load(atomic::Ordering::Relaxed);
    let home = Root { root: spec.home_trash.clone(), relative_to: None };
    // Made now, so the first item trashed later is noticed by the watch.
    let _ = trash::ensure_dirs(&home.root);
    if let Some(w) = watch {
        w.set_dirs_unless(&[home.files()], stale);
    }
    let mut l = Listing { dir: req.dir.clone(), fd: None, names: Vec::new(), slots: Vec::new(), meta: Vec::new(), complete: false, read_error: None, slow_stat: req.slow_stat, trash: None, recent: None };
    let mut t = TrashRows { roots: Vec::new(), fds: Vec::new(), root_of: Vec::new(), file_names: Vec::new(), info: Vec::new() };
    match read_trash_root(&home, req.slow_read, cancel) {
        Ok((file, entries)) => add_trash_root(&mut l, &mut t, home, file, entries),
        // Only the home Trash is always expected to be there.
        Err(e) => l.read_error = Some(describe(&e)),
    }
    let (tx, rx) = mpsc::channel();
    let (spec2, cancel2, slow) = (spec.clone(), cancel.clone(), req.slow_read);
    let spawned = std::thread::Builder::new().name("gannet-trash-disks".into()).spawn(move || {
        let mounts = spec2.mounts.clone().unwrap_or_else(read_mounts);
        let others: Vec<Root> = trash::roots(&spec2.home_trash, spec2.uid, &mounts).into_iter().skip(1).collect();
        let _ = tx.send(DiskMsg::Watch(others.iter().map(Root::files).collect()));
        for root in others {
            if cancel2.load(atomic::Ordering::Relaxed) {
                return;
            }
            if let Ok((file, entries)) = read_trash_root(&root, slow, &cancel2) {
                let _ = tx.send(DiskMsg::Root(root, file, entries));
            }
        }
    });
    if spawned.is_ok() {
        let started = Instant::now();
        // Rows on screen are behind what's been read: publish them at the next pause.
        let (mut published, mut behind) = (false, false);
        loop {
            if stale() {
                return;
            }
            let waited = started.elapsed();
            if waited >= spec.disks_give_up {
                // A disk that stops answering can keep the other thread stuck, but not this
                // one: the list finishes without it, and says so.
                if l.read_error.is_none() {
                    l.read_error = Some("a disk's Trash didn't answer in time".into());
                }
                break;
            }
            let wait = if published { Duration::from_millis(100) } else { RESOLVE_HEAD_START.saturating_sub(waited) };
            match rx.recv_timeout(wait.min(spec.disks_give_up - waited)) {
                Ok(DiskMsg::Watch(dirs)) => {
                    if let Some(w) = watch {
                        let all: Vec<PathBuf> = std::iter::once(spec.home_trash.join("files")).chain(dirs).collect();
                        w.set_dirs_unless(&all, stale);
                    }
                }
                Ok(DiskMsg::Root(root, file, entries)) => {
                    add_trash_root(&mut l, &mut t, root, file, entries);
                    behind = true;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if !published || behind {
                        publish_partial(trash_snapshot(&l, &t), &req, cancel, sink);
                        (published, behind) = (true, false);
                    }
                }
            }
        }
    }
    if stale() {
        return;
    }
    l.complete = true;
    l.meta = l.slots.iter().map(|_| new_meta()).collect();
    l.trash = Some(Arc::new(t));
    publish_final(Arc::new(l), &req, cancel, sink);
}

/// Lists the recently used files that are still there, newest first by their dates of use.
/// The list's folder is watched, as apps replace the list rather than write into it.
fn load_recent(req: LoadRequest, watch: Option<&Arc<Watch>>, cancel: &Arc<AtomicBool>, sink: &LoadSink) {
    let stale = || cancel.load(atomic::Ordering::Relaxed);
    if let (Some(w), Some(parent)) = (watch, req.dir.parent()) {
        w.set_dirs_unless(&[parent], stale);
    }
    let mut l = Listing { dir: req.dir.clone(), fd: None, names: Vec::new(), slots: Vec::new(), meta: Vec::new(), complete: false, read_error: None, slow_stat: req.slow_stat, trash: None, recent: None };
    let found = match read_list(&req.dir) {
        Ok(bytes) => crate::recent::parse(&bytes),
        // No list yet just means nothing's been used.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => {
            l.read_error = Some(describe(&e));
            Vec::new()
        }
    };
    let mut paths = Vec::new();
    for (path, used) in found {
        if paths.len() >= crate::recent::LIMIT || stale() {
            break;
        }
        if let Some(d) = req.slow_read {
            std::thread::sleep(d);
        }
        let Some(name) = path.file_name().map(|n| n.as_bytes().to_vec()) else { continue };
        if name.len() > u16::MAX as usize || l.names.len() + name.len() > u32::MAX as usize {
            continue;
        }
        let flags = AtFlags::SYMLINK_NOFOLLOW | AtFlags::NO_AUTOMOUNT;
        let Ok(c) = CString::new(path.as_os_str().as_bytes()) else { continue };
        let (kind, size) = match statx(rustix::fs::CWD, c.as_c_str(), flags, StatxFlags::TYPE | StatxFlags::SIZE) {
            Ok(st) => {
                let kind = match FileType::from_raw_mode(st.stx_mode as _) {
                    FileType::RegularFile => Kind::File,
                    FileType::Directory => Kind::Dir,
                    FileType::Symlink => match statat(rustix::fs::CWD, c.as_c_str(), AtFlags::empty()) {
                        Ok(t) if FileType::from_raw_mode(t.st_mode as _) == FileType::Directory => Kind::LinkDir,
                        _ => Kind::Link,
                    },
                    _ => Kind::Other,
                };
                (kind, st.stx_size)
            }
            // A file that's gone, or whose folder is, isn't recent any more.
            Err(rustix::io::Errno::NOENT | rustix::io::Errno::NOTDIR) => continue,
            Err(_) => (Kind::Other, SIZE_FAILED),
        };
        l.slots.push(Slot { off: l.names.len() as u32, len: name.len() as u16, kind });
        l.names.extend_from_slice(&name);
        l.meta.push(Meta { size: AtomicU64::new(size), mtime: AtomicI64::new(used) });
        paths.push(path);
    }
    if stale() {
        return;
    }
    l.complete = true;
    l.recent = Some(Arc::new(paths));
    publish_final(Arc::new(l), &req, cancel, sink);
}

/// Reads the recent list, if it's a plain file of a size a list could be.
fn read_list(path: &Path) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    const MAX: u64 = 64 * 1024 * 1024;
    let f = OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK).open(path)?;
    let m = f.metadata()?;
    if !m.is_file() {
        return Err(std::io::Error::other("it isn't a file"));
    }
    let mut buf = Vec::with_capacity(m.len().min(MAX) as usize);
    f.take(MAX).read_to_end(&mut buf)?;
    Ok(buf)
}

fn trash_snapshot(l: &Listing, t: &TrashRows) -> Listing {
    let mut s = l.snapshot();
    s.trash = Some(Arc::new(t.clone()));
    s
}

/// Works out what symlinks point at, on a thread of its own. If that stalls, the rows
/// go out as they are in the meantime.
fn resolve_kinds(l: &mut Listing, unresolved: Vec<u32>, req: &LoadRequest, cancel: &Arc<AtomicBool>, sink: &LoadSink) {
    if unresolved.is_empty() {
        return;
    }
    let names: Vec<Vec<u8>> = unresolved.iter().map(|&i| l.name(i).to_vec()).collect();
    let Some(fd) = l.fd.clone() else { return };
    let slow = req.slow_stat;
    let (tx, rx) = mpsc::channel();
    let spawned = std::thread::Builder::new().name("gannet-links".into()).spawn(move || {
        if let Some(d) = slow {
            std::thread::sleep(d);
        }
        let kinds: Vec<Kind> = names.iter().map(|n| CString::new(n.as_slice()).map_or(Kind::Other, |n| resolved_kind(&fd, &n))).collect();
        let _ = tx.send(kinds);
    });
    if spawned.is_err() {
        return;
    }
    let kinds = match rx.recv_timeout(RESOLVE_HEAD_START) {
        Ok(k) => k,
        Err(_) => {
            publish_partial(l.snapshot(), req, cancel, sink);
            match rx.recv() {
                Ok(k) => k,
                Err(_) => return,
            }
        }
    };
    for (i, k) in unresolved.into_iter().zip(kinds) {
        l.slots[i as usize].kind = k;
    }
}

fn resolved_kind(dir: &File, name: &CStr) -> Kind {
    let Ok(st) = statat(dir.as_fd(), name, AtFlags::SYMLINK_NOFOLLOW) else { return Kind::Other };
    match FileType::from_raw_mode(st.st_mode as _) {
        FileType::RegularFile => Kind::File,
        FileType::Directory => Kind::Dir,
        FileType::Symlink => match statat(dir.as_fd(), name, AtFlags::empty()) {
            Ok(t) if FileType::from_raw_mode(t.st_mode as _) == FileType::Directory => Kind::LinkDir,
            _ => Kind::Link,
        },
        _ => Kind::Other,
    }
}

/// Partial results are always in name order: a size or time order would mean statting
/// everything read so far, again for each partial result, while the read waits.
fn publish_partial(l: Listing, req: &LoadRequest, cancel: &Arc<AtomicBool>, sink: &LoadSink) {
    let order = SortSpec { key: SortKey::Name, descending: false };
    emit(Arc::new(l), order, req, cancel, false, sink);
}

fn publish_final(l: Arc<Listing>, req: &LoadRequest, cancel: &Arc<AtomicBool>, sink: &LoadSink) {
    if req.sort.key != SortKey::Name {
        let (tx, rx) = mpsc::channel();
        let (l2, c2) = (l.clone(), cancel.clone());
        let spawned = std::thread::Builder::new().name("gannet-stat-all".into()).spawn(move || {
            l2.stat_all(&c2);
            let _ = tx.send(());
        });
        if spawned.is_err() {
            l.stat_all(cancel);
        } else if rx.recv_timeout(SORT_STAT_HEAD_START).is_err() {
            emit(l.clone(), SortSpec::default(), req, cancel, false, sink);
            let _ = rx.recv();
        }
    }
    emit(l, req.sort, req, cancel, true, sink);
}

fn emit(l: Arc<Listing>, order: SortSpec, req: &LoadRequest, cancel: &Arc<AtomicBool>, settled: bool, sink: &LoadSink) {
    let sorted = sort_known(&l, order);
    let rows = visible_rows(&l, &sorted, req.show_hidden, &req.filter);
    let start = req.first_row.min(rows.len());
    let end = (start + req.row_count).min(rows.len());
    if end > start {
        head_start_stats(&l, rows[start..end].to_vec());
    }
    if !cancel.load(atomic::Ordering::Relaxed) {
        sink(LoadEvent::Ready(Loaded { generation: req.generation, listing: l, sorted, rows, sort: req.sort, settled }));
    }
}

/// Stats the first screen on a thread that can be abandoned, waiting only briefly.
fn head_start_stats(l: &Arc<Listing>, ids: Vec<u32>) {
    let (tx, rx) = mpsc::channel();
    let l2 = l.clone();
    let spawned = std::thread::Builder::new().name("gannet-stat-first".into()).spawn(move || {
        l2.stat_entries(&ids);
        let _ = tx.send(());
    });
    if spawned.is_ok() {
        let _ = rx.recv_timeout(STAT_HEAD_START);
    }
}

/// A short, readable reason, without the "(os error N)" suffix.
pub fn describe(e: &std::io::Error) -> String {
    let s = e.to_string();
    match s.find(" (os error") {
        Some(i) => s[..i].to_string(),
        None => s,
    }
}

/// A thread that stats rows when asked, always working from the newest request.
/// Each listing gets its own, so one hung folder can't hold up the next.
pub struct Stater {
    tx: mpsc::Sender<Vec<u32>>,
}

impl Stater {
    /// `None` if the system can't start a thread; rows then just keep blank columns.
    pub fn new(listing: Arc<Listing>, on_progress: impl Fn() + Send + 'static) -> Option<Self> {
        let (tx, rx) = mpsc::channel::<Vec<u32>>();
        std::thread::Builder::new()
            .name("gannet-stat".into())
            .spawn(move || {
                let mut next = None;
                loop {
                    let ids = match next.take() {
                        Some(ids) => ids,
                        None => match rx.recv() {
                            Ok(ids) => ids,
                            Err(_) => return,
                        },
                    };
                    for chunk in ids.chunks(64) {
                        if listing.stat_entries(chunk) {
                            on_progress();
                        }
                        // Newer requests replace this one: the window moved on.
                        if let Some(newest) = rx.try_iter().last() {
                            next = Some(newest);
                            break;
                        }
                    }
                }
            })
            .ok()?;
        Some(Self { tx })
    }

    pub fn request(&self, ids: Vec<u32>) {
        let _ = self.tx.send(ids);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::Scratch;
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::path::Path;

    fn request(dir: &Path) -> LoadRequest {
        LoadRequest { dir: dir.into(), generation: 1, sort: SortSpec::default(), show_hidden: false, filter: String::new(), first_row: 0, row_count: 64, slow_read: None, slow_stat: None, trash: None, recent: false }
    }

    /// Runs a load to completion, returning every event in order.
    fn run(req: LoadRequest) -> Vec<LoadEvent> {
        let (tx, rx) = mpsc::channel();
        spawn_load(req, move |e| {
            let _ = tx.send(e);
        });
        let mut out = Vec::new();
        for e in rx.iter() {
            let done = match &e {
                LoadEvent::Ready(l) => l.settled,
                LoadEvent::Failed { .. } => true,
            };
            out.push(e);
            if done {
                break;
            }
        }
        out
    }

    fn last(mut events: Vec<LoadEvent>) -> Loaded {
        match events.pop().unwrap() {
            LoadEvent::Ready(l) => l,
            LoadEvent::Failed { error, .. } => panic!("{error}"),
        }
    }

    fn row_names(l: &Loaded) -> Vec<String> {
        l.rows.iter().map(|&i| String::from_utf8_lossy(l.listing.name(i)).into_owned()).collect()
    }

    #[test]
    fn folders_come_first_then_natural_order() {
        let s = Scratch::new();
        for n in ["b10", "b9", "A"] {
            s.file(n, b"");
        }
        s.dir("zdir");
        assert_eq!(row_names(&last(run(request(s.path())))), ["zdir", "A", "b9", "b10"]);
    }

    #[test]
    fn hidden_files_only_when_asked() {
        let s = Scratch::new();
        s.file(".secret", b"");
        s.file("plain", b"");
        assert_eq!(row_names(&last(run(request(s.path())))), ["plain"]);
        let mut r = request(s.path());
        r.show_hidden = true;
        assert_eq!(row_names(&last(run(r))), [".secret", "plain"]);
    }

    #[test]
    fn symlink_kinds() {
        let s = Scratch::new();
        let d = s.dir("real");
        let f = s.file("realfile", b"x");
        symlink(&d, s.path().join("to-dir")).unwrap();
        symlink(&f, s.path().join("to-file")).unwrap();
        symlink("/nonexistent/gannet", s.path().join("broken")).unwrap();
        let l = last(run(request(s.path())));
        let kind = |n: &str| l.listing.kind(l.listing.find(n.as_bytes()).unwrap());
        assert_eq!(kind("to-dir"), Kind::LinkDir);
        assert_eq!(kind("to-file"), Kind::Link);
        assert_eq!(kind("broken"), Kind::Link);
        assert_eq!(kind("real"), Kind::Dir);
    }

    #[test]
    fn missing_folder_reports_a_failure_not_a_panic() {
        let s = Scratch::new();
        let events = run(request(&s.path().join("gone")));
        assert!(matches!(&events[0], LoadEvent::Failed { error, .. } if error.contains("No such file")));
    }

    #[test]
    fn a_file_is_not_a_folder() {
        let s = Scratch::new();
        let f = s.file("x", b"");
        let events = run(request(&f));
        assert!(matches!(&events[0], LoadEvent::Failed { .. }));
    }

    #[test]
    fn first_rows_have_stats_and_the_rest_dont() {
        let s = Scratch::new();
        for i in 0..200 {
            s.file(&format!("f{i:03}"), b"12345");
        }
        let mut r = request(s.path());
        r.row_count = 10;
        let l = last(run(r));
        assert!(matches!(l.listing.stat(l.rows[0]), Stat::Known { size: 5, .. }));
        assert_eq!(l.listing.stat(l.rows[100]), Stat::Unknown);
    }

    #[test]
    fn sort_by_size_and_time() {
        let s = Scratch::new();
        s.file("big", &[0; 1000]);
        s.file("small", &[0; 1]);
        s.file("mid", &[0; 10]);
        s.dir("adir");
        let mut r = request(s.path());
        r.sort = SortSpec { key: SortKey::Size, descending: true };
        assert_eq!(row_names(&last(run(r))), ["adir", "big", "mid", "small"]);
        let mut r = request(s.path());
        r.sort = SortSpec { key: SortKey::Modified, descending: false };
        assert_eq!(last(run(r)).rows.len(), 4);
    }

    #[test]
    fn filter_is_case_insensitive_and_matches_non_ascii() {
        let s = Scratch::new();
        for n in ["Report.PDF", "résumé.txt", "other"] {
            s.file(n, b"");
        }
        let mut r = request(s.path());
        r.filter = "report".into();
        assert_eq!(row_names(&last(run(r))), ["Report.PDF"]);
        let mut r = request(s.path());
        r.filter = "RÉSUMÉ".into();
        assert_eq!(row_names(&last(run(r))), ["résumé.txt"]);
    }

    #[test]
    fn non_utf8_names_round_trip() {
        let s = Scratch::new();
        let raw = OsStr::from_bytes(b"bad\xff\xfename");
        std::fs::write(s.path().join(raw), b"").unwrap();
        let l = last(run(request(s.path())));
        assert_eq!(l.listing.os_name(l.rows[0]), raw);
        assert!(l.listing.path(l.rows[0]).exists());
    }

    #[test]
    fn slow_folders_publish_partial_results_before_the_end() {
        let s = Scratch::new();
        for i in 0..3000 {
            s.file(&format!("f{i}"), b"");
        }
        let mut r = request(s.path());
        r.slow_read = Some(Duration::from_millis(10));
        let events = run(r);
        assert!(events.len() >= 2, "expected partial results, got {} events", events.len());
        let LoadEvent::Ready(first) = &events[0] else { panic!() };
        assert!(!first.listing.complete);
        assert!(first.rows.len() < 3000);
        assert_eq!(last(events).rows.len(), 3000);
    }

    #[test]
    fn cancelling_stops_publishing() {
        let s = Scratch::new();
        for i in 0..3000 {
            s.file(&format!("f{i}"), b"");
        }
        let (tx, rx) = mpsc::channel();
        let mut r = request(s.path());
        r.slow_read = Some(Duration::from_millis(10));
        let h = spawn_load(r, move |e| {
            let _ = tx.send(e);
        });
        h.cancel();
        assert!(rx.recv_timeout(Duration::from_secs(5)).is_err());
    }

    #[test]
    fn stater_fills_in_requested_rows() {
        let s = Scratch::new();
        for i in 0..100 {
            s.file(&format!("f{i:03}"), b"abc");
        }
        let mut r = request(s.path());
        r.row_count = 0;
        let l = last(run(r));
        assert_eq!(l.listing.stat(l.rows[50]), Stat::Unknown);
        let (tx, rx) = mpsc::channel();
        let st = Stater::new(l.listing.clone(), move || {
            let _ = tx.send(());
        })
        .unwrap();
        st.request(vec![l.rows[50]]);
        rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(matches!(l.listing.stat(l.rows[50]), Stat::Known { size: 3, .. }));
    }

    #[test]
    fn a_fifo_fails_at_once_instead_of_hanging() {
        let s = Scratch::new();
        let fifo = s.path().join("pipe");
        let c = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: the path is a valid C string; mkfifo reads it and returns.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let (tx, rx) = mpsc::channel();
        spawn_load(request(&fifo), move |e| {
            let _ = tx.send(e);
        });
        let got = rx.recv_timeout(Duration::from_secs(3)).expect("opening a FIFO must not block");
        assert!(matches!(got, LoadEvent::Failed { .. }));
    }

    #[test]
    fn a_stalled_symlink_doesnt_hold_back_the_rows() {
        let s = Scratch::new();
        s.file("plain", b"");
        let d = s.dir("real");
        symlink(&d, s.path().join("lnk")).unwrap();
        let mut r = request(s.path());
        r.slow_stat = Some(Duration::from_millis(1500));
        let (tx, rx) = mpsc::channel();
        let start = Instant::now();
        spawn_load(r, move |e| {
            let _ = tx.send(e);
        });
        let Ok(LoadEvent::Ready(first)) = rx.recv_timeout(Duration::from_secs(5)) else { panic!("no result") };
        assert!(start.elapsed() < Duration::from_millis(1000), "rows took {:?}", start.elapsed());
        assert!(!first.settled);
        assert_eq!(first.rows.len(), 3);
        assert_eq!(first.listing.kind(first.listing.find(b"lnk").unwrap()), Kind::Link);
        let last = loop {
            match rx.recv_timeout(Duration::from_secs(10)).unwrap() {
                LoadEvent::Ready(l) if l.settled => break l,
                _ => {}
            }
        };
        assert_eq!(last.listing.kind(last.listing.find(b"lnk").unwrap()), Kind::LinkDir);
    }

    #[test]
    fn a_stalled_stat_doesnt_hold_back_the_rows() {
        let s = Scratch::new();
        s.file("a", b"");
        let mut r = request(s.path());
        r.slow_stat = Some(Duration::from_millis(1500));
        let (tx, rx) = mpsc::channel();
        let start = Instant::now();
        spawn_load(r, move |e| {
            let _ = tx.send(e);
        });
        let Ok(LoadEvent::Ready(first)) = rx.recv_timeout(Duration::from_secs(5)) else { panic!("no result") };
        assert!(start.elapsed() < Duration::from_millis(1000), "rows took {:?}", start.elapsed());
        assert_eq!(first.listing.stat(first.rows[0]), Stat::Unknown);
    }

    #[test]
    fn a_stalled_sort_by_size_shows_rows_in_name_order_first() {
        let s = Scratch::new();
        s.file("a", &[0; 1]);
        s.file("b", &[0; 9]);
        let mut r = request(s.path());
        r.sort = SortSpec { key: SortKey::Size, descending: true };
        r.row_count = 0;
        r.slow_stat = Some(Duration::from_millis(800));
        let events = run(r);
        let LoadEvent::Ready(first) = &events[0] else { panic!() };
        assert!(!first.settled);
        assert_eq!(row_names(first), ["a", "b"]);
        assert_eq!(row_names(&last(events)), ["b", "a"]);
    }

    #[test]
    fn partial_results_dont_stat_the_whole_folder() {
        let s = Scratch::new();
        for i in 0..1500 {
            s.file(&format!("f{i}"), b"");
        }
        let mut r = request(s.path());
        r.sort = SortSpec { key: SortKey::Size, descending: false };
        r.row_count = 0;
        r.slow_read = Some(Duration::from_millis(10));
        let events = run(r);
        let LoadEvent::Ready(first) = &events[0] else { panic!() };
        assert!(!first.settled);
        assert!(first.rows.iter().all(|&i| first.listing.stat(i) == Stat::Unknown));
    }

    #[test]
    fn slow_small_folders_still_publish_and_cancel() {
        // Fewer than 1024 entries, so a check only every 1024 entries would never run.
        let s = Scratch::new();
        for i in 0..900 {
            s.file(&format!("f{i}"), b"");
        }
        let mut r = request(s.path());
        r.slow_read = Some(Duration::from_millis(10));
        let events = run(r);
        assert!(events.len() >= 2, "expected partial results, got {}", events.len());
    }

    #[test]
    fn an_empty_needle_matches_everything() {
        assert!(name_contains(b"abc", ""));
        assert!(name_contains(b"", ""));
        assert!(name_contains("é".as_bytes(), ""));
    }

    #[test]
    fn no_threads_left_means_a_failure_not_a_panic() {
        // In a child process, because the limit applies to the whole process.
        // SAFETY: the child only sets a limit, runs the load and exits; the parent waits.
        unsafe {
            let pid = libc::fork();
            assert!(pid >= 0);
            if pid == 0 {
                let lim = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
                libc::setrlimit(libc::RLIMIT_NPROC, &lim);
                let (tx, rx) = mpsc::channel();
                let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    spawn_load(request(std::path::Path::new("/")), move |e| {
                        let _ = tx.send(matches!(e, LoadEvent::Failed { .. }));
                    });
                }));
                let ok = ran.is_ok() && rx.recv_timeout(Duration::from_secs(5)) == Ok(true);
                libc::_exit(if ok { 0 } else { 1 });
            }
            let mut status = 0;
            libc::waitpid(pid, &mut status, 0);
            assert!(libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0, "status {status}");
        }
    }

    #[test]
    fn large_listing_is_sorted_and_complete() {
        // Built from fewer real files than the bench fixture to keep the test quick.
        let s = Scratch::new();
        for i in 0..20_000 {
            std::fs::File::create(s.path().join(format!("f{i}"))).unwrap();
        }
        let l = last(run(request(s.path())));
        assert_eq!(l.rows.len(), 20_000);
        assert!(l.rows.windows(2).all(|w| natural_cmp(l.listing.name(w[0]), l.listing.name(w[1])) == Ordering::Less));
    }

    #[test]
    fn the_trash_lists_every_trash_folder_under_original_names() {
        let s = Scratch::new();
        let uid = rustix::process::geteuid().as_raw();
        let home = s.path().join("data/Trash");
        let disk = s.dir("disk");
        let cfg = |ignore_home: bool| {
            let mut c = trash::TrashConfig::new(&s.path().join("data"), uid);
            let top = disk.clone();
            c.mount_of = Arc::new(move |p: &Path| p.starts_with(&top).then(|| top.clone()));
            c.ignore_home_trash = ignore_home;
            c
        };
        let a1 = trash::trash(&cfg(false), &s.file("work/a.txt", b"1")).unwrap();
        let a2 = trash::trash(&cfg(false), &s.file("work/a.txt", b"22")).unwrap();
        let far = trash::trash(&cfg(true), &s.file("disk/sub/far.txt", b"333")).unwrap();
        // An item with no info file still shows, under its own name, but can't be restored.
        fs::write(home.join("files/stray"), b"").unwrap();
        let mut req = request(&home.join("files"));
        req.trash = Some(TrashSpec { home_trash: home.clone(), uid, disks_give_up: DISKS_GIVE_UP, mounts: Some(vec![crate::mounts::Mount { point: disk.clone(), fs_type: "ext4".into(), source: "/dev/sdz1".into() }]) });
        let l = last(run(req));
        assert!(l.listing.is_trash());
        assert_eq!(row_names(&l), ["a.txt", "a.txt", "far.txt", "stray"]);
        let by_path = |p: &Path| (0..l.listing.len() as u32).find(|&i| l.listing.path(i) == p).unwrap();
        for t in [&a1, &a2, &far] {
            let i = by_path(&t.file);
            assert_eq!(l.listing.trashed(i).as_ref(), Some(t));
            assert_eq!(l.listing.original(i), Some(t.original.as_path()));
        }
        let stray = by_path(&home.join("files/stray"));
        assert_eq!(l.listing.trashed(stray), None);
        // Sizes come from the item where it is now, and the date from when it was trashed.
        let i = by_path(&far.file);
        l.listing.stat_entries(&[i]);
        let deleted = trash::parse_info(&fs::read(&far.info).unwrap(), Some(&disk)).unwrap().deleted.unwrap();
        assert_eq!(l.listing.stat(i), Stat::Known { size: 3, mtime: deleted });
        // The disk's Trash folder is held open too, which matters for unmounting it.
        let held = l.listing.held_dirs();
        assert!(held.iter().any(|d| d.starts_with(&disk)), "{held:?}");
    }

    #[test]
    fn recent_lists_files_that_are_still_there_newest_first() {
        let s = Scratch::new();
        let (old, new, dir) = (s.file("a/old.txt", b"1"), s.file("b/new.txt", b"22"), s.dir("c/folder"));
        let same_name = s.file("c/old.txt", b"333");
        let uri = |p: &Path| crate::uri::to_file_uri(p);
        let xbel = format!(
            "<xbel>\n<bookmark href=\"{}\" visited=\"2026-01-01T00:00:00Z\"/>\n<bookmark href=\"{}\" visited=\"2026-03-01T00:00:00Z\"/>\n<bookmark href=\"{}\" visited=\"2026-02-01T00:00:00Z\"/>\n<bookmark href=\"{}\" visited=\"2026-04-01T00:00:00Z\"/>\n<bookmark href=\"{}\" visited=\"2026-05-01T00:00:00Z\"/>\n</xbel>",
            uri(&old), uri(&new), uri(&dir), uri(&s.path().join("gone.txt")), uri(&same_name)
        );
        let list = s.file("data/recently-used.xbel", xbel.as_bytes());
        let mut req = request(&list);
        req.recent = true;
        req.sort = SortSpec { key: SortKey::Modified, descending: true };
        let l = last(run(req));
        assert!(l.listing.is_recent() && l.listing.read_error.is_none());
        assert_eq!(row_names(&l), ["old.txt", "new.txt", "folder", "old.txt"], "newest first, the missing one left out");
        let paths: Vec<_> = l.rows.iter().map(|&i| l.listing.path(i)).collect();
        assert_eq!(paths, [same_name.clone(), new.clone(), dir.clone(), old.clone()]);
        assert_eq!(l.listing.location(l.rows[0]), Some(s.path().join("c").as_path()));
        assert!(l.listing.kind(l.rows[2]).is_dirlike());
        assert_eq!(l.listing.stat(l.rows[1]), Stat::Known { size: 2, mtime: crate::recent::parse_utc("2026-03-01T00:00:00Z").unwrap() }, "the date is when it was used");
        assert_ne!(l.listing.key(l.rows[0]), l.listing.key(l.rows[3]), "two files of one name stay apart");
        // No list yet is an empty Recent, not an error.
        let mut req = request(&s.path().join("data/none.xbel"));
        req.recent = true;
        let l = last(run(req));
        assert!(l.rows.is_empty() && l.listing.read_error.is_none());
    }

    #[test]
    fn a_folder_reached_through_a_symlink_says_where_it_really_is() {
        let s = Scratch::new();
        let real = s.dir("media/STICK");
        s.file("media/STICK/a", b"");
        symlink(&real, s.path().join("stick")).unwrap();
        let l = last(run(request(&s.path().join("stick"))));
        let held = l.listing.held_dirs();
        assert!(held.contains(&s.path().join("stick")), "as it was named: {held:?}");
        assert!(held.contains(&fs::canonicalize(&real).unwrap()), "and where it really is: {held:?}");
    }

    #[test]
    fn an_empty_trash_is_made_rather_than_failing() {
        let s = Scratch::new();
        let home = s.path().join("data/Trash");
        let mut req = request(&home.join("files"));
        req.trash = Some(TrashSpec { home_trash: home.clone(), uid: 1000, disks_give_up: DISKS_GIVE_UP, mounts: Some(Vec::new()) });
        let l = last(run(req));
        assert!(l.listing.is_empty() && l.listing.read_error.is_none());
        assert!(home.join("info").is_dir());
    }

    fn disk_mount(point: &Path) -> crate::mounts::Mount {
        crate::mounts::Mount { point: point.to_path_buf(), fs_type: "ext4".into(), source: "/dev/sdz1".into() }
    }

    #[test]
    fn a_slow_disk_doesnt_hold_up_the_home_trash() {
        let s = Scratch::new();
        let uid = rustix::process::geteuid().as_raw();
        let home = s.path().join("data/Trash");
        let disk = s.dir("disk");
        s.file(&format!("disk/.Trash-{uid}/files/far"), b"x");
        let mut req = request(&home.join("files"));
        // Each entry read waits this long, and only the other disk has one.
        req.slow_read = Some(Duration::from_millis(400));
        req.trash = Some(TrashSpec { home_trash: home.clone(), uid, disks_give_up: DISKS_GIVE_UP, mounts: Some(vec![disk_mount(&disk)]) });
        let events = run(req);
        let Some(LoadEvent::Ready(first)) = events.first() else { panic!("no rows") };
        assert!(!first.settled && first.listing.is_empty() && first.listing.is_trash(), "the home Trash shows first");
        let l = last(events);
        assert_eq!(row_names(&l), ["far"]);
    }

    #[test]
    fn a_fifo_for_an_info_file_doesnt_hang_the_trash() {
        let s = Scratch::new();
        let home = s.path().join("data/Trash");
        s.file("data/Trash/files/x", b"");
        fs::create_dir_all(home.join("info")).unwrap();
        rustix::fs::mknodat(rustix::fs::CWD, home.join("info/x.trashinfo"), rustix::fs::FileType::Fifo, rustix::fs::Mode::from_raw_mode(0o600), 0).unwrap();
        let mut req = request(&home.join("files"));
        req.trash = Some(TrashSpec { home_trash: home.clone(), uid: 1000, disks_give_up: DISKS_GIVE_UP, mounts: Some(Vec::new()) });
        let (tx, rx) = mpsc::channel();
        spawn_load(req, move |e| {
            let _ = tx.send(e);
        });
        loop {
            match rx.recv_timeout(Duration::from_secs(5)).expect("the load must finish") {
                LoadEvent::Ready(l) if l.settled => {
                    assert_eq!(row_names(&l), ["x"]);
                    assert!(l.listing.trashed(l.rows[0]).is_none());
                    break;
                }
                LoadEvent::Ready(_) => {}
                LoadEvent::Failed { error, .. } => panic!("{error}"),
            }
        }
    }

    #[test]
    fn rows_sharing_a_name_have_their_own_keys() {
        let s = Scratch::new();
        let home = s.path().join("data/Trash");
        let mut c = trash::TrashConfig::new(&s.path().join("data"), 1000);
        c.mount_of = Arc::new(|_| None);
        trash::trash(&c, &s.file("work/a", b"1")).unwrap();
        trash::trash(&c, &s.file("work/a", b"2")).unwrap();
        let mut req = request(&home.join("files"));
        req.trash = Some(TrashSpec { home_trash: home, uid: 1000, disks_give_up: DISKS_GIVE_UP, mounts: Some(Vec::new()) });
        let l = last(run(req));
        assert_eq!(row_names(&l), ["a", "a"]);
        assert_ne!(l.listing.key(0), l.listing.key(1));
    }

    #[test]
    fn a_disk_that_never_answers_doesnt_stop_the_trash_finishing() {
        let s = Scratch::new();
        let uid = rustix::process::geteuid().as_raw();
        let home = s.path().join("data/Trash");
        let disk = s.dir("disk");
        s.file(&format!("disk/.Trash-{uid}/files/far"), b"");
        let mut req = request(&home.join("files"));
        // Each entry read waits a minute, and only the other disk has one: it stands in
        // for a disk that's stopped answering.
        req.slow_read = Some(Duration::from_secs(60));
        req.trash = Some(TrashSpec { home_trash: home.clone(), uid, disks_give_up: Duration::from_millis(300), mounts: Some(vec![disk_mount(&disk)]) });
        let (tx, rx) = mpsc::channel();
        spawn_load(req, move |e| {
            let _ = tx.send(e);
        });
        let started = Instant::now();
        loop {
            match rx.recv_timeout(Duration::from_secs(10)).expect("the load must finish") {
                LoadEvent::Ready(l) if l.settled => {
                    assert!(l.listing.is_empty());
                    assert!(l.listing.read_error.as_deref().is_some_and(|e| e.contains("didn't answer")));
                    break;
                }
                LoadEvent::Ready(_) => {}
                LoadEvent::Failed { error, .. } => panic!("{error}"),
            }
        }
        assert!(started.elapsed() < Duration::from_secs(10), "finished without the other disk");
    }
}
