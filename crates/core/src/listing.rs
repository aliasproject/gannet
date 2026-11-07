//! Folder listings, built on worker threads and handed to the UI whole.
//!
//! A listing is immutable once published, except that sizes and times start unknown
//! and fill in through atomics as they're needed. Only visible rows get a stat unless
//! a sort needs every row, so big folders on slow disks stay cheap. A hung disk can
//! hang only its own worker thread: nothing waits for one.

use std::cmp::Ordering;
use std::ffi::{CStr, CString, OsStr};
use std::fs::{File, OpenOptions};
use std::mem::MaybeUninit;
use std::os::fd::AsFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::sync::atomic::{self, AtomicBool, AtomicI64, AtomicU64, AtomicUsize};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use rustix::fs::{AtFlags, FileType, RawDir, StatxFlags, statat, statx};

use crate::sort::{natural_cmp, par_sort_by};
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
}

impl Listing {
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
        self.dir.join(self.os_name(i))
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
        let Some(fd) = self.fd.as_ref() else {
            m.size.store(SIZE_FAILED, atomic::Ordering::Relaxed);
            return;
        };
        let name = self.name(i);
        let flags = AtFlags::SYMLINK_NOFOLLOW | AtFlags::NO_AUTOMOUNT;
        let got = std::ffi::CString::new(name).ok().and_then(|n| statx(fd.as_fd(), n.as_c_str(), flags, StatxFlags::SIZE | StatxFlags::MTIME).ok());
        match got {
            Some(st) => {
                // Release pairs with the Acquire in stat(): a reader that sees the size sees the time.
                m.mtime.store(st.stx_mtime.tv_sec, atomic::Ordering::Relaxed);
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
        let (da, db) = (l.kind(a).is_dirlike(), l.kind(b).is_dirlike());
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
}

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
        if let Some(w) = &watch {
            w.set_dirs_unless(std::slice::from_ref(&dir), || c.load(atomic::Ordering::Relaxed));
        }
        let done = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| load(req, &c, &s2)));
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
    let mut l = Listing { dir: req.dir.clone(), fd: Some(file.clone()), names: Vec::new(), slots: Vec::new(), meta: Vec::new(), complete: false, read_error: None, slow_stat: req.slow_stat };
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
    use std::os::unix::fs::symlink;
    use std::path::Path;

    fn request(dir: &Path) -> LoadRequest {
        LoadRequest { dir: dir.into(), generation: 1, sort: SortSpec::default(), show_hidden: false, filter: String::new(), first_row: 0, row_count: 64, slow_read: None, slow_stat: None }
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
}
