//! File operations, each on its own thread, so nothing here can block the UI.
//!
//! Rules this module keeps:
//! - A file never appears at its destination half-written. Copies go to a temporary
//!   name and get renamed into place; cancel or failure removes the temporary file.
//! - Nothing gets overwritten unless the user chose Replace for it.
//! - Symlinks are copied and moved as symlinks, never followed.
//! - One item failing doesn't stop the others. Each failure gets reported.
//! - A move across disks removes each source only after its copy succeeded.
//! - Folders never get replaced wholesale. Replace on two folders means merge.
//! - Delete and cross-disk move stop at mount points inside a folder: another disk's
//!   contents are never removed because it happens to be mounted there.
//! - A cross-disk move syncs each copy to disk before removing its source.

use std::ffi::{OsStr, OsString};
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, IntoRawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use rustix::fs::{AtFlags, CWD, RenameFlags, StatxAttributes, StatxFlags, Timespec, Timestamps, XattrFlags, copy_file_range, fgetxattr, flistxattr, fsetxattr, futimens, renameat_with, sendfile, statx, syncfs};
use rustix::io::Errno;

use crate::listing::describe;
use crate::trash::{self, MountCache, TrashConfig, TrashError, Trashed};

#[derive(Clone, Debug)]
pub enum Request {
    Copy { sources: Vec<PathBuf>, dest: PathBuf },
    Move { sources: Vec<PathBuf>, dest: PathBuf },
    /// Makes a symbolic link in `dest` to each source.
    Link { sources: Vec<PathBuf>, dest: PathBuf },
    Trash { paths: Vec<PathBuf> },
    /// Deletes for good. The UI must have asked first.
    Delete { paths: Vec<PathBuf> },
    Rename { from: PathBuf, to_name: OsString },
    NewFolder { parent: PathBuf, name: OsString },
    Undo(Undo),
}

/// How to reverse a finished operation.
#[derive(Clone, Debug, PartialEq)]
pub enum Undo {
    Rename { from: PathBuf, to: PathBuf },
    Moves(Vec<(PathBuf, PathBuf)>),
    Trash(Vec<Trashed>),
}

#[derive(Default)]
pub struct Progress {
    pub total_bytes: AtomicU64,
    pub done_bytes: AtomicU64,
    pub total_items: AtomicU64,
    pub done_items: AtomicU64,
    /// Still measuring; totals are only a lower bound.
    pub scanning: AtomicBool,
    pub current: Mutex<String>,
}

#[derive(Clone)]
pub struct Job {
    pub id: u64,
    pub label: String,
    pub progress: Arc<Progress>,
    cancel: Arc<AtomicBool>,
}

impl Job {
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Choice {
    Replace,
    Skip,
    KeepBoth,
    Cancel,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Answer {
    pub choice: Choice,
    pub apply_to_all: bool,
}

pub struct Conflict {
    pub job: u64,
    pub source: PathBuf,
    pub target: PathBuf,
    /// Both folders: Replace will merge.
    pub both_folders: bool,
    /// A file and a folder: Replace isn't offered.
    pub can_replace: bool,
    pub reply: mpsc::SyncSender<Answer>,
}

pub struct Outcome {
    pub job: u64,
    pub cancelled: bool,
    pub errors: usize,
    /// Items left where they were because of an answer to a conflict, or part of a merge.
    pub skipped: usize,
    pub undo: Option<Undo>,
    /// New top-level paths, so the UI can select them.
    pub created: Vec<PathBuf>,
    /// Folders that changed, so the UI can refresh them if inotify can't see them.
    pub touched: Vec<PathBuf>,
}

pub enum Event {
    /// Totals moved. Throttled to about 20 per second per job.
    Progress(u64),
    Conflict(Conflict),
    Error { job: u64, path: PathBuf, message: String },
    Finished(Outcome),
}

pub type Sink = Arc<dyn Fn(Event) + Send + Sync>;

#[derive(Clone)]
pub struct Engine {
    pub trash: TrashConfig,
    sink: Sink,
    next_id: Arc<AtomicU64>,
    /// Test seam: make every rename between folders fail like a move across disks.
    pub force_exdev: bool,
    /// Test seam: make syncing a moved file fail, as a full or removed disk would.
    pub force_sync_error: bool,
    /// Test seam: copy with read and write, as on a kernel that refuses `sendfile`.
    pub force_no_sendfile: bool,
}

impl Engine {
    pub fn new(trash: TrashConfig, sink: Sink) -> Self {
        Self { trash, sink, next_id: Arc::new(AtomicU64::new(1)), force_exdev: false, force_sync_error: false, force_no_sendfile: false }
    }

    pub fn start(&self, request: Request) -> Job {
        let request = clean_request(request);
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let job = Job { id, label: label_for(&request), progress: Arc::new(Progress::default()), cancel: Arc::new(AtomicBool::new(false)) };
        let mut run = Run {
            job: job.clone(),
            sink: self.sink.clone(),
            trash: self.trash.clone(),
            force_exdev: self.force_exdev,
            force_sync_error: self.force_sync_error,
            force_no_sendfile: self.force_no_sendfile,
            errors: 0,
            noted_leftovers: 0,
            sticky_files: None,
            sticky_folders: None,
            last_merged: false,
            top_dev: 0,
            undo_moves: Vec::new(),
            last_target: None,
            depth: 0,
            created: Vec::new(),
            touched: Vec::new(),
            last_progress: Instant::now() - Duration::from_secs(1),
        };
        std::thread::Builder::new()
            .name("gannet-op".into())
            .spawn(move || {
                let undo = run.execute(request);
                let outcome = Outcome {
                    job: run.job.id,
                    cancelled: run.job.is_cancelled(),
                    errors: run.errors,
                    skipped: run.noted_leftovers,
                    undo,
                    created: std::mem::take(&mut run.created),
                    touched: std::mem::take(&mut run.touched),
                };
                (run.sink)(Event::Finished(outcome));
            })
            .expect("spawn operation thread");
        job
    }
}

/// Removes trailing slashes and `.` parts. They matter: `lstat("link/")` follows the
/// link, which would defeat every no-follow rule below.
fn clean_path(p: PathBuf) -> PathBuf {
    p.components().collect()
}

fn clean_request(r: Request) -> Request {
    let many = |v: Vec<PathBuf>| v.into_iter().map(clean_path).collect::<Vec<_>>();
    match r {
        Request::Copy { sources, dest } => Request::Copy { sources: many(sources), dest: clean_path(dest) },
        Request::Move { sources, dest } => Request::Move { sources: many(sources), dest: clean_path(dest) },
        Request::Link { sources, dest } => Request::Link { sources: many(sources), dest: clean_path(dest) },
        Request::Trash { paths } => Request::Trash { paths: many(paths) },
        Request::Delete { paths } => Request::Delete { paths: many(paths) },
        Request::Rename { from, to_name } => Request::Rename { from: clean_path(from), to_name },
        Request::NewFolder { parent, name } => Request::NewFolder { parent: clean_path(parent), name },
        Request::Undo(u) => Request::Undo(u),
    }
}

fn label_for(r: &Request) -> String {
    fn count(paths: &[PathBuf]) -> String {
        match paths {
            [one] => format!("“{}”", one.file_name().unwrap_or(OsStr::new("/")).to_string_lossy()),
            many => format!("{} items", many.len()),
        }
    }
    fn folder(p: &Path) -> String {
        p.file_name().unwrap_or(p.as_os_str()).to_string_lossy().into_owned()
    }
    match r {
        Request::Copy { sources, dest } => format!("Copying {} to {}", count(sources), folder(dest)),
        Request::Move { sources, dest } => format!("Moving {} to {}", count(sources), folder(dest)),
        Request::Link { sources, dest } => format!("Linking {} in {}", count(sources), folder(dest)),
        Request::Trash { paths } => format!("Moving {} to Trash", count(paths)),
        Request::Delete { paths } => format!("Deleting {}", count(paths)),
        Request::Rename { from, to_name } => format!("Renaming {} to “{}”", count(std::slice::from_ref(from)), to_name.to_string_lossy()),
        Request::NewFolder { name, .. } => format!("Creating “{}”", name.to_string_lossy()),
        Request::Undo(_) => "Undoing".to_string(),
    }
}

/// A name the app may create: one path component, not `.` or `..`, not too long.
pub fn validate_name(name: &str) -> Result<(), &'static str> {
    if name.is_empty() {
        return Err("The name can't be empty");
    }
    if name == "." || name == ".." {
        return Err("That name is reserved");
    }
    if name.contains('/') {
        return Err("A name can't contain “/”");
    }
    if name.contains('\0') {
        return Err("A name can't contain a null character");
    }
    if name.len() > 255 {
        return Err("That name is too long");
    }
    Ok(())
}

/// What a drag does when it's dropped on a folder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DropVerb {
    Move,
    Copy,
    Link,
}

/// The keys held at the drop decide: Ctrl copies, Shift moves, and both make a link. With
/// neither, a drag moves on the same disk and copies to another. It also copies when the
/// disks can't be told apart (`same_disk` is `None`), or when the source can't be deleted.
pub fn drop_verb(ctrl: bool, shift: bool, same_disk: Option<bool>, can_delete: bool) -> DropVerb {
    match (ctrl, shift) {
        (true, true) => DropVerb::Link,
        (true, false) => DropVerb::Copy,
        (false, true) => DropVerb::Move,
        (false, false) if same_disk == Some(true) && can_delete => DropVerb::Move,
        (false, false) => DropVerb::Copy,
    }
}

/// How far `sendfile_rest` got. Each holds the bytes sent, except `Cancelled`.
#[derive(Debug, PartialEq)]
enum Sent {
    /// Reached the end of the input.
    Complete(u64),
    /// `sendfile` can't do this pair, or found nothing to send. Pseudo files can report
    /// that and still have data, so the rest has to be read and written.
    Unsupported(u64),
    Cancelled,
}

/// Copies from the input's current position to its end. Faster than reading and writing
/// when `copy_file_range` won't cross filesystems. `chunk_done` gets each chunk's size
/// and returns false to stop.
fn sendfile_rest(input: &File, output: &File, mut chunk_done: impl FnMut(u64) -> bool) -> io::Result<Sent> {
    const CHUNK: usize = 8 * 1024 * 1024;
    let mut sent = 0u64;
    loop {
        match sendfile(output, input, None, CHUNK) {
            Ok(0) if sent == 0 => return Ok(Sent::Unsupported(0)),
            Ok(0) => return Ok(Sent::Complete(sent)),
            Ok(n) => {
                sent += n as u64;
                if !chunk_done(n as u64) {
                    return Ok(Sent::Cancelled);
                }
            }
            Err(Errno::INTR) => continue,
            Err(Errno::INVAL | Errno::NOSYS | Errno::OPNOTSUPP | Errno::BADF | Errno::PERM) => return Ok(Sent::Unsupported(sent)),
            Err(e) => return Err(e.into()),
        }
    }
}

/// Renames without replacing. Filesystems without `RENAME_NOREPLACE` fall back to a
/// check first, which has a small race but is the best they allow.
pub fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    match renameat_with(CWD, from, CWD, to, RenameFlags::NOREPLACE) {
        Ok(()) => Ok(()),
        Err(Errno::INVAL | Errno::NOSYS) => {
            if fs::symlink_metadata(to).is_ok() {
                return Err(io::Error::from(io::ErrorKind::AlreadyExists));
            }
            fs::rename(from, to)
        }
        Err(e) => Err(e.into()),
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Copy,
    Move,
}

enum Flow {
    Go,
    Stop,
}

struct Run {
    job: Job,
    sink: Sink,
    trash: TrashConfig,
    force_exdev: bool,
    force_sync_error: bool,
    force_no_sendfile: bool,
    errors: usize,
    /// Items left in place on purpose, so a folder that won't empty isn't a surprise.
    noted_leftovers: usize,
    /// The answers given with "apply to all". Folders and files are kept apart, because
    /// Replace on a folder only merges, and that must never authorize overwriting files.
    sticky_files: Option<Answer>,
    sticky_folders: Option<Answer>,
    last_merged: bool,
    top_dev: u64,
    undo_moves: Vec<(PathBuf, PathBuf)>,
    /// Where the current top-level item is going.
    last_target: Option<PathBuf>,
    depth: usize,
    created: Vec<PathBuf>,
    touched: Vec<PathBuf>,
    last_progress: Instant,
}

impl Run {
    fn execute(&mut self, request: Request) -> Option<Undo> {
        match request {
            Request::Copy { sources, dest } => {
                self.scan(&sources);
                self.place_all(&sources, &dest, Mode::Copy);
                None
            }
            Request::Move { sources, dest } => {
                self.job.progress.total_items.store(sources.len() as u64, Ordering::Relaxed);
                self.place_all(&sources, &dest, Mode::Move);
                let moves = std::mem::take(&mut self.undo_moves);
                (!moves.is_empty()).then_some(Undo::Moves(moves))
            }
            Request::Link { sources, dest } => {
                self.job.progress.total_items.store(sources.len() as u64, Ordering::Relaxed);
                self.link_all(&sources, &dest);
                None
            }
            Request::Trash { paths } => self.trash_all(paths),
            Request::Delete { paths } => {
                self.scan(&paths);
                for p in &paths {
                    if self.job.is_cancelled() {
                        break;
                    }
                    if let Ok(m) = fs::symlink_metadata(p) {
                        self.top_dev = m.dev();
                    }
                    self.delete_tree(p);
                    if fs::symlink_metadata(p).is_err()
                        && let Some(info) = trash::info_file_for(&self.trash, p)
                    {
                        let _ = fs::remove_file(info);
                    }
                    self.touch_parent(p);
                }
                None
            }
            Request::Rename { from, to_name } => self.rename(from, to_name),
            Request::NewFolder { parent, name } => {
                let target = parent.join(&name);
                match validate_name(&name.to_string_lossy()).map_err(io::Error::other).and_then(|_| fs::create_dir(&target)) {
                    Ok(()) => {
                        self.created.push(target);
                        self.touched.push(parent);
                    }
                    Err(e) => self.fail(&target, &e),
                }
                None
            }
            Request::Undo(u) => {
                self.undo(u);
                None
            }
        }
    }

    fn rename(&mut self, from: PathBuf, to_name: OsString) -> Option<Undo> {
        if let Err(msg) = validate_name(&to_name.to_string_lossy()) {
            self.fail(&from, &io::Error::other(msg));
            return None;
        }
        let parent = from.parent().unwrap_or(Path::new("/")).to_path_buf();
        let to = parent.join(&to_name);
        match rename_noreplace(&from, &to) {
            Ok(()) => {
                self.created.push(to.clone());
                self.touched.push(parent);
                Some(Undo::Rename { from, to })
            }
            Err(e) => {
                self.fail(&from, &e);
                None
            }
        }
    }

    fn trash_all(&mut self, paths: Vec<PathBuf>) -> Option<Undo> {
        self.job.progress.total_items.store(paths.len() as u64, Ordering::Relaxed);
        let mut done: Vec<Trashed> = Vec::new();
        let mut mounts = MountCache::default();
        for p in &paths {
            if self.job.is_cancelled() {
                break;
            }
            match trash::trash_in_batch(&self.trash, p, &mut mounts) {
                Ok(t) => done.push(t),
                Err(TrashError::Io(e)) => self.fail(p, &e),
                Err(e) => self.report(p, e.to_string()),
            }
            self.touch_parent(p);
            self.job.progress.done_items.fetch_add(1, Ordering::Relaxed);
            self.tick();
        }
        self.sync_trash(&done);
        (!done.is_empty()).then_some(Undo::Trash(done))
    }

    /// Saves a finished batch to disk, once for each Trash folder it went to. The items
    /// have already moved, so a failure is reported and they stay in the Trash.
    fn sync_trash(&mut self, done: &[Trashed]) {
        let mut folders: Vec<&Path> = Vec::new();
        for t in done {
            if let Some(folder) = t.info.parent()
                && !folders.contains(&folder)
            {
                folders.push(folder);
            }
        }
        for folder in folders {
            let synced = if self.force_sync_error { Err(Errno::IO.into()) } else { File::open(folder).and_then(|d| syncfs(&d).map_err(io::Error::from)) };
            if let Err(e) = synced {
                self.report(folder, format!("Moved to the Trash, but it may not be saved to disk yet: {}", describe(&e)));
            }
        }
    }

    fn undo(&mut self, u: Undo) {
        match u {
            Undo::Rename { from, to } => match rename_noreplace(&to, &from) {
                Ok(()) => {
                    self.created.push(from.clone());
                    self.touch_parent(&from);
                }
                Err(e) => self.fail(&to, &e),
            },
            Undo::Moves(moves) => {
                self.job.progress.total_items.store(moves.len() as u64, Ordering::Relaxed);
                for (from, to) in moves.into_iter().rev() {
                    if self.job.is_cancelled() {
                        break;
                    }
                    self.last_target = None;
                    let flow = self.place(&to, from.clone(), Mode::Move);
                    self.record_top_level(&to, Mode::Move);
                    self.touch_parent(&from);
                    self.touch_parent(&to);
                    self.job.progress.done_items.fetch_add(1, Ordering::Relaxed);
                    if let Flow::Stop = flow {
                        break;
                    }
                }
            }
            Undo::Trash(items) => {
                for t in items {
                    match trash::restore(&t) {
                        Ok(()) => {
                            self.created.push(t.original.clone());
                            self.touch_parent(&t.original);
                        }
                        Err(e) => self.fail(&t.original, &e),
                    }
                }
            }
        }
    }

    fn touch_parent(&mut self, p: &Path) {
        if let Some(parent) = p.parent()
            && !self.touched.iter().any(|t| t == parent)
        {
            self.touched.push(parent.to_path_buf());
        }
    }

    fn fail(&mut self, path: &Path, e: &io::Error) {
        self.report(path, describe(e));
    }

    fn report(&mut self, path: &Path, message: String) {
        self.errors += 1;
        (self.sink)(Event::Error { job: self.job.id, path: path.to_path_buf(), message });
    }

    fn tick(&mut self) {
        if self.last_progress.elapsed() >= Duration::from_millis(50) {
            self.last_progress = Instant::now();
            (self.sink)(Event::Progress(self.job.id));
        }
    }

    fn set_current(&self, p: &Path) {
        if let Ok(mut c) = self.job.progress.current.lock() {
            *c = p.file_name().unwrap_or(p.as_os_str()).to_string_lossy().into_owned();
        }
    }

    /// Adds up the bytes and items so progress has totals.
    fn scan(&mut self, paths: &[PathBuf]) {
        let progress = self.job.progress.clone();
        progress.scanning.store(true, Ordering::Relaxed);
        let mut stack: Vec<PathBuf> = paths.to_vec();
        while let Some(p) = stack.pop() {
            if self.job.is_cancelled() {
                break;
            }
            let Ok(m) = fs::symlink_metadata(&p) else { continue };
            progress.total_items.fetch_add(1, Ordering::Relaxed);
            if m.is_dir() {
                if let Ok(rd) = fs::read_dir(&p) {
                    stack.extend(rd.flatten().map(|e| e.path()));
                }
            } else {
                progress.total_bytes.fetch_add(m.len(), Ordering::Relaxed);
            }
            self.tick();
        }
        progress.scanning.store(false, Ordering::Relaxed);
    }

    fn link_all(&mut self, sources: &[PathBuf], dest: &Path) {
        // First, so a job stopped partway still reports the folder its links are in.
        self.touched.push(dest.to_path_buf());
        for src in sources {
            if self.job.is_cancelled() {
                return;
            }
            self.set_current(src);
            if !self.link_one(src, dest) {
                return;
            }
            self.job.progress.done_items.fetch_add(1, Ordering::Relaxed);
            self.tick();
        }
    }

    /// Links one item. Returns false if the whole job should stop.
    fn link_one(&mut self, src: &Path, dest: &Path) -> bool {
        let (Some(name), Some(parent)) = (src.file_name(), src.parent()) else {
            self.report(src, "This can't be linked".into());
            return true;
        };
        // The folder is resolved and the item isn't, so a link to a symlink points at that symlink.
        let original = match fs::canonicalize(parent) {
            Ok(parent) => parent.join(name),
            Err(e) => {
                self.fail(src, &e);
                return true;
            }
        };
        let mut target = dest.join(name);
        if fs::symlink_metadata(&target).is_ok() {
            if same_location(src, &target) {
                target = unique_copy_name(&target);
            } else {
                match self.ask(src, &target, false, false) {
                    Choice::Cancel => {
                        self.job.cancel();
                        return false;
                    }
                    Choice::KeepBoth => target = unique_copy_name(&target),
                    // A link never replaces anything, so a stray Replace is a Skip too.
                    Choice::Skip | Choice::Replace => {
                        self.noted_leftovers += 1;
                        return true;
                    }
                }
            }
        }
        match std::os::unix::fs::symlink(&original, &target) {
            Ok(()) => self.created.push(target),
            Err(e) => self.fail(&target, &e),
        }
        true
    }

    fn place_all(&mut self, sources: &[PathBuf], dest: &Path, mode: Mode) {
        for src in sources {
            if self.job.is_cancelled() {
                return;
            }
            let Some(name) = src.file_name() else {
                self.report(src, "This can't be copied or moved".into());
                continue;
            };
            if is_inside_itself(src, dest) {
                self.report(src, "A folder can't go inside itself".into());
                continue;
            }
            let target = dest.join(name);
            self.last_target = None;
            let flow = self.place(src, target, mode);
            self.record_top_level(src, mode);
            if mode == Mode::Move {
                self.touch_parent(src);
                self.job.progress.done_items.fetch_add(1, Ordering::Relaxed);
            }
            if !self.touched.iter().any(|t| t == dest) {
                self.touched.push(dest.to_path_buf());
            }
            self.tick();
            if let Flow::Stop = flow {
                return;
            }
        }
    }

    /// Notes where a top-level item ended up. Only top-level items go in the undo list
    /// and the list of things to select: undoing a folder's move brings its contents.
    fn record_top_level(&mut self, src: &Path, mode: Mode) {
        let merged = std::mem::take(&mut self.last_merged);
        let Some(target) = self.last_target.take() else { return };
        if fs::symlink_metadata(&target).is_err() {
            return;
        }
        // Undoing a merge would also take what was in the destination before, so it has no undo.
        if mode == Mode::Move && !merged && fs::symlink_metadata(src).is_err() {
            self.undo_moves.push((src.to_path_buf(), target.clone()));
        }
        self.created.push(target);
    }

    /// Puts `src` at `target`, asking about conflicts. `Stop` means the job was cancelled.
    fn place(&mut self, src: &Path, mut target: PathBuf, mode: Mode) -> Flow {
        if self.job.is_cancelled() {
            return Flow::Stop;
        }
        self.set_current(src);
        let smeta = match fs::symlink_metadata(src) {
            Ok(m) => m,
            Err(e) => {
                self.fail(src, &e);
                return Flow::Go;
            }
        };
        if self.depth == 0 {
            self.top_dev = smeta.dev();
        } else if mode == Mode::Move && smeta.is_dir() && is_mount_boundary(src, &smeta, self.top_dev) {
            self.report(src, "This is a mount point, so it was left where it is".into());
            return Flow::Go;
        }
        let mut merge = false;
        let mut replace = false;
        if let Ok(tmeta) = fs::symlink_metadata(&target) {
            if same_location(src, &target) {
                // Pasted onto itself. A copy gets a new name; a move has nothing to do, and the
                // item didn't go anywhere, which whoever asked may need to know.
                if mode == Mode::Move {
                    self.noted_leftovers += 1;
                    return Flow::Go;
                }
                target = unique_copy_name(&target);
            } else {
                let both_dirs = smeta.is_dir() && tmeta.is_dir();
                let can_replace = smeta.is_dir() == tmeta.is_dir();
                match self.ask(src, &target, both_dirs, can_replace) {
                    Choice::Cancel => {
                        self.job.cancel();
                        return Flow::Stop;
                    }
                    Choice::Skip => {
                        self.noted_leftovers += 1;
                        self.count_skipped(&smeta);
                        return Flow::Go;
                    }
                    Choice::KeepBoth => target = unique_copy_name(&target),
                    Choice::Replace if both_dirs => merge = true,
                    Choice::Replace if can_replace => replace = true,
                    // The UI doesn't offer this; treat a stray answer as Skip.
                    Choice::Replace => {
                        self.noted_leftovers += 1;
                        self.count_skipped(&smeta);
                        return Flow::Go;
                    }
                }
            }
        }
        if self.depth == 0 {
            self.last_target = Some(target.clone());
            self.last_merged = merge;
        }
        if mode == Mode::Move && !merge {
            match self.try_rename(src, &target, replace) {
                Ok(()) => {
                    self.job.progress.done_bytes.fetch_add(if smeta.is_dir() { 0 } else { smeta.len() }, Ordering::Relaxed);
                    return Flow::Go;
                }
                Err(e) if e.raw_os_error() == Some(Errno::XDEV.raw_os_error()) => {
                    // Across disks: copy then remove, measuring what's about to move. The
                    // top item's scan covers everything below it.
                    if self.depth == 0 {
                        self.scan(&[src.to_path_buf()]);
                    }
                }
                Err(e) => {
                    self.fail(src, &e);
                    return Flow::Go;
                }
            }
        }
        let ft = smeta.file_type();
        if ft.is_dir() {
            self.copy_dir(src, &target, &smeta, mode, merge)
        } else if ft.is_symlink() {
            self.copy_symlink(src, &target, replace, mode)
        } else if ft.is_file() {
            self.copy_file(src, &target, &smeta, replace, mode)
        } else {
            self.report(src, "Special files like sockets and devices aren't copied".into());
            Flow::Go
        }
    }

    fn try_rename(&self, src: &Path, target: &Path, replace: bool) -> io::Result<()> {
        if self.force_exdev {
            return Err(Errno::XDEV.into());
        }
        if !replace {
            return rename_noreplace(src, target);
        }
        fs::rename(src, target)?;
        // Renaming over another hard link to the same file does nothing and reports
        // success, leaving the source there.
        if let (Ok(a), Ok(b)) = (fs::symlink_metadata(src), fs::symlink_metadata(target))
            && a.dev() == b.dev()
            && a.ino() == b.ino()
        {
            fs::remove_file(src)?;
        }
        Ok(())
    }

    fn count_skipped(&self, m: &Metadata) {
        if !m.is_dir() {
            self.job.progress.done_bytes.fetch_add(m.len(), Ordering::Relaxed);
        }
    }

    fn ask(&mut self, src: &Path, target: &Path, both_folders: bool, can_replace: bool) -> Choice {
        let sticky = if both_folders { self.sticky_folders } else { self.sticky_files };
        if let Some(a) = sticky {
            // A sticky Replace can't apply where Replace isn't possible.
            if a.choice != Choice::Replace || can_replace {
                return a.choice;
            }
        }
        let (tx, rx) = mpsc::sync_channel(1);
        (self.sink)(Event::Conflict(Conflict { job: self.job.id, source: src.to_path_buf(), target: target.to_path_buf(), both_folders, can_replace, reply: tx }));
        loop {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(a) => {
                    if a.apply_to_all {
                        if both_folders {
                            self.sticky_folders = Some(a);
                        } else {
                            self.sticky_files = Some(a);
                        }
                    }
                    return a.choice;
                }
                Err(mpsc::RecvTimeoutError::Timeout) if !self.job.is_cancelled() => {}
                // Cancelled, or the UI went away: stop rather than guess.
                Err(_) => return Choice::Cancel,
            }
        }
    }

    fn copy_dir(&mut self, src: &Path, target: &Path, smeta: &Metadata, mode: Mode, merging: bool) -> Flow {
        let mut created = false;
        if !merging {
            // Writable while we fill it; the real mode goes on at the end.
            let mut b = fs::DirBuilder::new();
            b.mode(smeta.permissions().mode() & 0o7777 | 0o700);
            if let Err(e) = b.create(target) {
                self.fail(target, &e);
                return Flow::Go;
            }
            created = true;
        }
        let entries = match fs::read_dir(src) {
            Ok(rd) => rd,
            Err(e) => {
                self.fail(src, &e);
                // Nothing went in, and an empty folder would look like a successful copy.
                if created {
                    let _ = fs::remove_dir(target);
                }
                return Flow::Go;
            }
        };
        let leftovers_before = self.errors + self.noted_leftovers;
        for e in entries {
            let e = match e {
                Ok(e) => e,
                Err(err) => {
                    self.fail(src, &err);
                    continue;
                }
            };
            self.depth += 1;
            let flow = self.place(&e.path(), target.join(e.file_name()), mode);
            self.depth -= 1;
            if let Flow::Stop = flow {
                if created {
                    finish_folder(target, smeta);
                }
                return Flow::Stop;
            }
        }
        if created {
            finish_folder(target, smeta);
        }
        if mode == Mode::Move {
            // Fails only if something stayed behind (skipped or errored). That's right: it stays.
            match fs::remove_dir(src) {
                Ok(()) => {}
                // Expected if something below was skipped or failed. If not, something
                // new appeared while we worked, and that deserves a mention.
                Err(e) if e.raw_os_error() == Some(Errno::NOTEMPTY.raw_os_error()) => {
                    if self.errors + self.noted_leftovers == leftovers_before {
                        self.report(src, "Something new appeared here during the move, so the folder was kept".into());
                    }
                }
                Err(e) => self.fail(src, &e),
            }
        }
        self.job.progress.done_items.fetch_add(u64::from(mode == Mode::Copy), Ordering::Relaxed);
        self.tick();
        Flow::Go
    }

    fn copy_symlink(&mut self, src: &Path, target: &Path, replace: bool, mode: Mode) -> Flow {
        let result = fs::read_link(src).and_then(|dest| {
            let tmp = temp_name(target);
            std::os::unix::fs::symlink(&dest, &tmp)?;
            let moved = if replace { fs::rename(&tmp, target) } else { rename_noreplace(&tmp, target) };
            if moved.is_err() {
                let _ = fs::remove_file(&tmp);
            }
            moved
        });
        match result {
            Ok(()) => self.finish_item(src, target, mode),
            Err(e) => self.fail(target, &e),
        }
        Flow::Go
    }

    /// Bookkeeping once something is fully in place.
    fn finish_item(&mut self, src: &Path, target: &Path, mode: Mode) {
        if mode == Mode::Move {
            // The name must be on disk too before the only other copy goes away.
            let synced = target.parent().map_or(Ok(()), |p| File::open(p).and_then(|d| self.sync_for_move(&d)));
            if let Err(e) = synced {
                self.report(src, format!("Copied, but couldn't confirm it reached the disk, so the original stays: {}", describe(&e)));
                return;
            }
            if let Err(e) = fs::remove_file(src) {
                self.fail(src, &e);
            }
        } else {
            self.job.progress.done_items.fetch_add(1, Ordering::Relaxed);
        }
        self.tick();
    }

    fn copy_file(&mut self, src: &Path, target: &Path, smeta: &Metadata, replace: bool, mode: Mode) -> Flow {
        let tmp = temp_name(target);
        match self.copy_contents(src, &tmp, smeta, mode) {
            Ok(true) => {}
            Ok(false) => {
                let _ = fs::remove_file(&tmp);
                return Flow::Stop;
            }
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                self.fail(src, &e);
                return Flow::Go;
            }
        }
        let placed = if replace { fs::rename(&tmp, target) } else { rename_noreplace(&tmp, target) };
        match placed {
            Ok(()) => self.finish_item(src, target, mode),
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                self.fail(target, &e);
            }
        }
        Flow::Go
    }

    /// Copies bytes and metadata to `tmp`. `Ok(false)` means cancelled.
    fn copy_contents(&mut self, src: &Path, tmp: &Path, smeta: &Metadata, mode: Mode) -> io::Result<bool> {
        let input = OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(src)?;
        let output = OpenOptions::new().write(true).create_new(true).mode(0o600).open(tmp)?;
        let mut copied = 0u64;
        let mut fallback = false;
        const CHUNK: usize = 8 * 1024 * 1024;
        let progress = self.job.progress.clone();
        loop {
            if self.job.is_cancelled() {
                return Ok(false);
            }
            match copy_file_range(&input, None, &output, None, CHUNK) {
                Ok(0) => break,
                Ok(n) => {
                    copied += n as u64;
                    progress.done_bytes.fetch_add(n as u64, Ordering::Relaxed);
                    self.tick();
                }
                Err(Errno::XDEV | Errno::INVAL | Errno::NOSYS | Errno::OPNOTSUPP | Errno::BADF | Errno::PERM) => {
                    fallback = true;
                    break;
                }
                Err(Errno::INTR) => continue,
                Err(e) => return Err(e.into()),
            }
        }
        // Some pseudo filesystems report no bytes from copy_file_range but have data.
        if fallback || copied == 0 {
            let mut complete = false;
            if !self.force_no_sendfile {
                let sent = sendfile_rest(&input, &output, |n| {
                    progress.done_bytes.fetch_add(n, Ordering::Relaxed);
                    self.tick();
                    !self.job.is_cancelled()
                })?;
                match sent {
                    Sent::Cancelled => return Ok(false),
                    Sent::Complete(n) => {
                        copied += n;
                        complete = true;
                    }
                    Sent::Unsupported(n) => copied += n,
                }
            }
            if !complete {
                let mut input = &input;
                let mut out = &output;
                let mut buf = vec![0u8; 1024 * 1024];
                loop {
                    if self.job.is_cancelled() {
                        return Ok(false);
                    }
                    let n = match input.read(&mut buf) {
                        Ok(n) => n,
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                        Err(e) => return Err(e),
                    };
                    if n == 0 {
                        break;
                    }
                    out.write_all(&buf[..n])?;
                    copied += n as u64;
                    progress.done_bytes.fetch_add(n as u64, Ordering::Relaxed);
                    self.tick();
                }
            }
        }
        if copied < smeta.len() {
            return Err(io::Error::other("The file got shorter while it was being copied"));
        }
        copy_xattrs(&input, &output);
        // Setuid and setgid are dropped: the copy belongs to whoever made it. Some
        // filesystems (FAT, some FUSE) refuse chmod, and that's no reason to fail.
        match output.set_permissions(fs::Permissions::from_mode(smeta.permissions().mode() & 0o777)) {
            Err(e) if !matches!(e.raw_os_error().map(Errno::from_raw_os_error), Some(Errno::PERM | Errno::OPNOTSUPP)) => return Err(e),
            _ => {}
        }
        let _ = set_mtime(&output, smeta);
        if mode == Mode::Move {
            self.sync_for_move(&output)?;
        }
        close_checked(output)?;
        Ok(true)
    }

    fn sync_for_move(&self, f: &File) -> io::Result<()> {
        if self.force_sync_error {
            return Err(Errno::IO.into());
        }
        f.sync_all()
    }

    fn delete_tree(&mut self, path: &Path) {
        if self.job.is_cancelled() {
            return;
        }
        self.set_current(path);
        let meta = match fs::symlink_metadata(path) {
            Ok(m) => m,
            Err(e) => {
                self.fail(path, &e);
                return;
            }
        };
        if meta.is_dir() {
            if is_mount_boundary(path, &meta, self.top_dev) {
                self.report(path, "This is a mount point, so its contents were not deleted".into());
                return;
            }
            let errors_before = self.errors;
            match fs::read_dir(path) {
                Ok(rd) => {
                    for e in rd {
                        match e {
                            Ok(e) => self.delete_tree(&e.path()),
                            Err(err) => self.fail(path, &err),
                        }
                    }
                }
                Err(e) => {
                    self.fail(path, &e);
                    return;
                }
            }
            if self.job.is_cancelled() {
                return;
            }
            if let Err(e) = fs::remove_dir(path) {
                let not_empty = e.raw_os_error() == Some(Errno::NOTEMPTY.raw_os_error());
                if !not_empty {
                    self.fail(path, &e);
                } else if self.errors == errors_before {
                    self.report(path, "Something new appeared here during the delete, so the folder was kept".into());
                }
            }
        } else {
            if let Err(e) = fs::remove_file(path) {
                self.fail(path, &e);
            }
            self.job.progress.done_bytes.fetch_add(meta.len(), Ordering::Relaxed);
        }
        self.job.progress.done_items.fetch_add(1, Ordering::Relaxed);
        self.tick();
    }
}

/// True if `path`, a folder below a top-level item on device `top_dev`, is a mount point.
/// The device check catches ordinary mounts and the attribute catches bind mounts.
fn is_mount_boundary(path: &Path, m: &Metadata, top_dev: u64) -> bool {
    if m.dev() != top_dev {
        return true;
    }
    statx(CWD, path, AtFlags::SYMLINK_NOFOLLOW, StatxFlags::BASIC_STATS).is_ok_and(|st| st.stx_attributes_mask.contains(StatxAttributes::MOUNT_ROOT) && st.stx_attributes.contains(StatxAttributes::MOUNT_ROOT))
}

/// True if both paths name the same directory entry. Two hard links are one file but
/// two entries, so comparing inodes would wrongly call them the same place.
fn same_location(a: &Path, b: &Path) -> bool {
    if a.file_name() != b.file_name() {
        return false;
    }
    let canon = |p: &Path| p.parent().filter(|d| !d.as_os_str().is_empty()).and_then(|d| fs::canonicalize(d).ok());
    matches!((canon(a), canon(b)), (Some(x), Some(y)) if x == y)
}

/// Restores what a finished or interrupted folder copy should have.
fn finish_folder(target: &Path, smeta: &Metadata) {
    // Keep sticky on folders, drop setuid and setgid.
    let _ = fs::set_permissions(target, fs::Permissions::from_mode(smeta.permissions().mode() & 0o1777));
    if let Ok(f) = File::open(target) {
        let _ = set_mtime(&f, smeta);
    }
}

/// Copies extended attributes, which also carry ACLs. Best effort: not every
/// filesystem takes them, and security labels often can't be set.
fn copy_xattrs(from: &File, to: &File) {
    let mut names = vec![0u8; 4096];
    let Ok(n) = flistxattr(from, &mut names) else { return };
    let mut value = vec![0u8; 64 * 1024];
    for name in names[..n].split(|&b| b == 0).filter(|b| !b.is_empty()) {
        let Ok(name) = std::ffi::CString::new(name) else { continue };
        if let Ok(len) = fgetxattr(from, name.as_c_str(), &mut value) {
            let _ = fsetxattr(to, name.as_c_str(), &value[..len], XattrFlags::empty());
        }
    }
}

/// Closes a file and reports the error, which network filesystems can save for now.
fn close_checked(f: File) -> io::Result<()> {
    let fd = f.into_raw_fd();
    // SAFETY: into_raw_fd gave us sole ownership of fd, and it's closed exactly once.
    if unsafe { libc::close(fd) } != 0 {
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
    Ok(())
}

fn set_mtime(f: &File, m: &Metadata) -> io::Result<()> {
    let times = Timestamps {
        last_access: Timespec { tv_sec: m.atime(), tv_nsec: m.atime_nsec() as _ },
        last_modification: Timespec { tv_sec: m.mtime(), tv_nsec: m.mtime_nsec() as _ },
    };
    futimens(f.as_fd(), &times).map_err(Into::into)
}

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_name(target: &Path) -> PathBuf {
    let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    target.with_file_name(format!(".gannet-partial-{}-{n}", std::process::id()))
}

/// True if `dest` is `src` or anywhere below it. Follows symlinks on both sides, except
/// that a symlink source is only itself: copying a link never recurses into its target.
fn is_inside_itself(src: &Path, dest: &Path) -> bool {
    let Ok(m) = fs::symlink_metadata(src) else { return false };
    if !m.is_dir() {
        return false;
    }
    match (fs::canonicalize(src), fs::canonicalize(dest)) {
        (Ok(a), Ok(b)) => b.starts_with(a),
        _ => false,
    }
}

/// The largest length up to `max` that doesn't cut a UTF-8 character in half.
pub(crate) fn char_boundary_at_most(b: &[u8], max: usize) -> usize {
    if b.len() <= max {
        return b.len();
    }
    let mut n = max;
    while n > 0 && b[n] & 0xc0 == 0x80 {
        n -= 1;
    }
    n
}

/// `report.pdf` -> `report (copy).pdf`, then `report (copy 2).pdf`, and so on.
pub fn unique_copy_name(target: &Path) -> PathBuf {
    let name = target.file_name().map_or(&b""[..], |n| n.as_bytes());
    let dot = name.iter().rposition(|&c| c == b'.').filter(|&i| i > 0);
    // Folders and dotfiles don't have extensions to protect.
    let is_dir = fs::symlink_metadata(target).is_ok_and(|m| m.is_dir());
    let (stem, ext) = match dot {
        Some(i) if !is_dir => (&name[..i], &name[i..]),
        _ => (name, &b""[..]),
    };
    for n in 1u32.. {
        let suffix = if n == 1 { " (copy)".to_string() } else { format!(" (copy {n})") };
        let mut out = stem.to_vec();
        // Keep within the 255-byte limit by shortening the stem.
        let room = 255usize.saturating_sub(suffix.len() + ext.len());
        out.truncate(char_boundary_at_most(stem, room));
        out.extend_from_slice(suffix.as_bytes());
        out.extend_from_slice(ext);
        let candidate = target.with_file_name(OsString::from_vec(out));
        if fs::symlink_metadata(&candidate).is_err() {
            return candidate;
        }
    }
    unreachable!()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::Scratch;
    use std::os::unix::fs::symlink;
    use std::sync::mpsc::{Receiver, channel};

    /// An engine whose events come out of a channel. Its Trash lives in the scratch
    /// folder and it sees no real mounts.
    fn engine(s: &Scratch) -> (Engine, Receiver<Event>) {
        let (tx, rx) = channel();
        let tx = Mutex::new(tx);
        let mut trash = TrashConfig::new(&s.path().join("data"), 1000);
        trash.mount_of = Arc::new(|_| None);
        (
            Engine::new(
                trash,
                Arc::new(move |e| {
                    let _ = tx.lock().unwrap().send(e);
                }),
            ),
            rx,
        )
    }

    struct Done {
        outcome: Outcome,
        errors: Vec<(PathBuf, String)>,
    }

    /// Waits for the job, answering conflicts from `answers` and failing the test on
    /// any conflict that wasn't expected.
    fn finish(rx: &Receiver<Event>, mut answers: Vec<Answer>) -> Done {
        let mut errors = Vec::new();
        loop {
            match rx.recv_timeout(Duration::from_secs(20)).expect("job should finish") {
                Event::Progress(_) => {}
                Event::Error { path, message, .. } => errors.push((path, message)),
                Event::Conflict(c) => {
                    assert!(!answers.is_empty(), "unexpected conflict on {:?}", c.target);
                    c.reply.send(answers.remove(0)).unwrap();
                }
                Event::Finished(outcome) => {
                    assert!(answers.is_empty(), "conflicts expected but never asked");
                    return Done { outcome, errors };
                }
            }
        }
    }

    fn answer(choice: Choice) -> Answer {
        Answer { choice, apply_to_all: false }
    }

    fn all(choice: Choice) -> Answer {
        Answer { choice, apply_to_all: true }
    }

    fn no_partials(dir: &Path) {
        for e in fs::read_dir(dir).unwrap().flatten() {
            assert!(!e.file_name().to_string_lossy().starts_with(".gannet-partial"), "partial file left: {:?}", e.path());
            if e.file_type().unwrap().is_dir() {
                no_partials(&e.path());
            }
        }
    }

    #[test]
    fn copies_a_file_with_mode_and_time() {
        let s = Scratch::new();
        let (e, rx) = engine(&s);
        let f = s.file("a.sh", b"#!/bin/sh\n");
        fs::set_permissions(&f, fs::Permissions::from_mode(0o751)).unwrap();
        let old = File::open(&f).unwrap();
        futimens(old.as_fd(), &Timestamps { last_access: Timespec { tv_sec: 1000, tv_nsec: 0 }, last_modification: Timespec { tv_sec: 2000, tv_nsec: 0 } }).unwrap();
        let d = s.dir("dest");
        e.start(Request::Copy { sources: vec![f.clone()], dest: d.clone() });
        let done = finish(&rx, vec![]);
        assert!(done.errors.is_empty());
        let m = fs::metadata(d.join("a.sh")).unwrap();
        assert_eq!(m.permissions().mode() & 0o777, 0o751);
        assert_eq!(m.mtime(), 2000);
        assert_eq!(fs::read(d.join("a.sh")).unwrap(), b"#!/bin/sh\n");
        assert!(f.exists());
        assert_eq!(done.outcome.created, vec![d.join("a.sh")]);
        no_partials(&d);
    }

    #[test]
    fn copies_a_tree_including_symlinks_and_empty_files() {
        let s = Scratch::new();
        let (e, rx) = engine(&s);
        s.file("src/x/y/deep", b"deep");
        s.file("src/empty", b"");
        s.dir("src/emptydir");
        symlink("x/y/deep", s.path().join("src/link")).unwrap();
        symlink("/nonexistent/gannet", s.path().join("src/broken")).unwrap();
        let d = s.dir("dest");
        e.start(Request::Copy { sources: vec![s.path().join("src")], dest: d.clone() });
        let done = finish(&rx, vec![]);
        assert!(done.errors.is_empty(), "{:?}", done.errors);
        assert_eq!(fs::read(d.join("src/x/y/deep")).unwrap(), b"deep");
        assert_eq!(fs::read(d.join("src/empty")).unwrap(), b"");
        assert!(d.join("src/emptydir").is_dir());
        assert_eq!(fs::read_link(d.join("src/link")).unwrap(), Path::new("x/y/deep"));
        assert_eq!(fs::read_link(d.join("src/broken")).unwrap(), Path::new("/nonexistent/gannet"));
        no_partials(&d);
    }

    #[test]
    fn folder_mode_is_restored_after_filling_it() {
        let s = Scratch::new();
        let (e, rx) = engine(&s);
        let src = s.dir("src");
        s.file("src/f", b"x");
        fs::set_permissions(&src, fs::Permissions::from_mode(0o555)).unwrap();
        let d = s.dir("dest");
        e.start(Request::Copy { sources: vec![src.clone()], dest: d.clone() });
        let done = finish(&rx, vec![]);
        fs::set_permissions(&src, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(done.errors.is_empty());
        let mode = fs::metadata(d.join("src")).unwrap().permissions().mode() & 0o777;
        fs::set_permissions(d.join("src"), fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(mode, 0o555);
    }

    #[test]
    fn copying_onto_itself_makes_a_named_copy() {
        let s = Scratch::new();
        let (e, rx) = engine(&s);
        let f = s.file("doc.txt", b"1");
        e.start(Request::Copy { sources: vec![f.clone()], dest: s.path().to_path_buf() });
        finish(&rx, vec![]);
        assert!(s.path().join("doc (copy).txt").exists());
        e.start(Request::Copy { sources: vec![f], dest: s.path().to_path_buf() });
        finish(&rx, vec![]);
        assert!(s.path().join("doc (copy 2).txt").exists());
    }

    #[test]
    fn moving_onto_itself_does_nothing() {
        let s = Scratch::new();
        let (e, rx) = engine(&s);
        let f = s.file("doc.txt", b"1");
        e.start(Request::Move { sources: vec![f.clone()], dest: s.path().to_path_buf() });
        let done = finish(&rx, vec![]);
        assert!(done.errors.is_empty());
        assert_eq!(fs::read(&f).unwrap(), b"1");
        assert_eq!(done.outcome.undo, None);
    }

    #[test]
    fn a_folder_cant_go_inside_itself() {
        let s = Scratch::new();
        let (e, rx) = engine(&s);
        let d = s.dir("top");
        let inner = s.dir("top/inner");
        for req in [Request::Copy { sources: vec![d.clone()], dest: inner.clone() }, Request::Move { sources: vec![d.clone()], dest: inner.clone() }] {
            e.start(req);
            let done = finish(&rx, vec![]);
            assert_eq!(done.errors.len(), 1);
        }
        assert!(d.is_dir());
        assert_eq!(fs::read_dir(&inner).unwrap().count(), 0);
    }

    fn conflict_setup(s: &Scratch) -> (PathBuf, PathBuf) {
        (s.file("src/a.txt", b"new"), s.file("dest/a.txt", b"old"))
    }

    #[test]
    fn conflict_replace_overwrites() {
        let s = Scratch::new();
        let (e, rx) = engine(&s);
        let (src, dst) = conflict_setup(&s);
        e.start(Request::Copy { sources: vec![src], dest: dst.parent().unwrap().into() });
        finish(&rx, vec![answer(Choice::Replace)]);
        assert_eq!(fs::read(&dst).unwrap(), b"new");
        no_partials(dst.parent().unwrap());
    }

    #[test]
    fn conflict_skip_leaves_the_target_alone() {
        let s = Scratch::new();
        let (e, rx) = engine(&s);
        let (src, dst) = conflict_setup(&s);
        e.start(Request::Copy { sources: vec![src], dest: dst.parent().unwrap().into() });
        finish(&rx, vec![answer(Choice::Skip)]);
        assert_eq!(fs::read(&dst).unwrap(), b"old");
    }

    #[test]
    fn conflict_keep_both_makes_a_copy_name() {
        let s = Scratch::new();
        let (e, rx) = engine(&s);
        let (src, dst) = conflict_setup(&s);
        e.start(Request::Copy { sources: vec![src], dest: dst.parent().unwrap().into() });
        finish(&rx, vec![answer(Choice::KeepBoth)]);
        assert_eq!(fs::read(&dst).unwrap(), b"old");
        assert_eq!(fs::read(s.path().join("dest/a (copy).txt")).unwrap(), b"new");
    }

    #[test]
    fn conflict_cancel_stops_the_job() {
        let s = Scratch::new();
        let (e, rx) = engine(&s);
        let (src, dst) = conflict_setup(&s);
        let other = s.file("src/z.txt", b"z");
        e.start(Request::Copy { sources: vec![src, other], dest: dst.parent().unwrap().into() });
        let done = finish(&rx, vec![answer(Choice::Cancel)]);
        assert!(done.outcome.cancelled);
        assert_eq!(fs::read(&dst).unwrap(), b"old");
        assert!(!s.path().join("dest/z.txt").exists());
    }

    #[test]
    fn apply_to_all_asks_only_once() {
        let s = Scratch::new();
        let (e, rx) = engine(&s);
        let mut sources = Vec::new();
        for n in ["a", "b", "c"] {
            sources.push(s.file(&format!("src/{n}"), b"new"));
            s.file(&format!("dest/{n}"), b"old");
        }
        e.start(Request::Copy { sources, dest: s.path().join("dest") });
        finish(&rx, vec![all(Choice::Replace)]);
        for n in ["a", "b", "c"] {
            assert_eq!(fs::read(s.path().join("dest").join(n)).unwrap(), b"new");
        }
    }

    #[test]
    fn two_folders_merge_on_replace_and_never_lose_files() {
        let s = Scratch::new();
        let (e, rx) = engine(&s);
        s.file("src/d/only-src", b"s");
        s.file("src/d/both", b"new");
        s.file("dest/d/only-dest", b"d");
        s.file("dest/d/both", b"old");
        e.start(Request::Copy { sources: vec![s.path().join("src/d")], dest: s.path().join("dest") });
        finish(&rx, vec![answer(Choice::Replace), answer(Choice::Skip)]);
        assert_eq!(fs::read(s.path().join("dest/d/only-dest")).unwrap(), b"d");
        assert_eq!(fs::read(s.path().join("dest/d/only-src")).unwrap(), b"s");
        assert_eq!(fs::read(s.path().join("dest/d/both")).unwrap(), b"old");
    }

    #[test]
    fn a_folder_never_replaces_a_file_or_the_reverse() {
        let s = Scratch::new();
        let (e, rx) = engine(&s);
        s.file("src/x/inner", b"i");
        let target = s.file("dest/x", b"file");
        e.start(Request::Copy { sources: vec![s.path().join("src/x")], dest: s.path().join("dest") });
        // A stray Replace (which the UI doesn't offer) must act as Skip.
        finish(&rx, vec![answer(Choice::Replace)]);
        assert_eq!(fs::read(&target).unwrap(), b"file");
        assert!(s.path().join("src/x/inner").exists());
    }

    #[test]
    fn a_sticky_replace_doesnt_apply_to_a_file_folder_clash() {
        let s = Scratch::new();
        let (e, rx) = engine(&s);
        let f1 = s.file("src/a", b"new");
        s.file("dest/a", b"old");
        let f2 = s.file("src/b", b"new");
        s.file("dest/b/inner", b"keep");
        e.start(Request::Copy { sources: vec![f1, f2], dest: s.path().join("dest") });
        finish(&rx, vec![all(Choice::Replace), answer(Choice::Skip)]);
        assert_eq!(fs::read(s.path().join("dest/b/inner")).unwrap(), b"keep");
    }

    #[test]
    fn unreadable_file_reports_and_the_rest_continue() {
        let s = Scratch::new();
        let (e, rx) = engine(&s);
        let bad = s.file("src/bad", b"x");
        let good = s.file("src/good", b"y");
        fs::set_permissions(&bad, fs::Permissions::from_mode(0o000)).unwrap();
        e.start(Request::Copy { sources: vec![bad.clone(), good], dest: s.dir("dest") });
        let done = finish(&rx, vec![]);
        fs::set_permissions(&bad, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(done.errors.len(), 1, "{:?}", done.errors);
        assert!(done.errors[0].1.contains("denied"));
        assert!(s.path().join("dest/good").exists());
        assert!(!s.path().join("dest/bad").exists());
        no_partials(&s.path().join("dest"));
    }

    #[test]
    fn read_only_destination_reports_and_leaves_nothing() {
        let s = Scratch::new();
        let (e, rx) = engine(&s);
        let f = s.file("a", b"x");
        let d = s.dir("ro");
        fs::set_permissions(&d, fs::Permissions::from_mode(0o500)).unwrap();
        e.start(Request::Copy { sources: vec![f.clone()], dest: d.clone() });
        let done = finish(&rx, vec![]);
        fs::set_permissions(&d, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(done.errors.len(), 1);
        assert_eq!(fs::read_dir(&d).unwrap().count(), 0);
        assert!(f.exists());
    }

    #[test]
    fn special_files_report_instead_of_hanging() {
        let s = Scratch::new();
        let (e, rx) = engine(&s);
        let fifo = s.path().join("pipe");
        let c = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: the path is a valid C string; mkfifo reads it and returns.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        e.start(Request::Copy { sources: vec![fifo], dest: s.dir("dest") });
        let done = finish(&rx, vec![]);
        assert_eq!(done.errors.len(), 1);
    }

    #[test]
    fn cancel_leaves_no_partial_file() {
        let s = Scratch::new();
        let (e, rx) = engine(&s);
        // Big enough that the cancel lands mid-copy on the read/write fallback path too.
        let f = s.path().join("big");
        File::create(&f).unwrap().set_len(1 << 33).unwrap();
        let d = s.dir("dest");
        let job = e.start(Request::Copy { sources: vec![f], dest: d.clone() });
        job.cancel();
        let done = finish(&rx, vec![]);
        assert!(done.outcome.cancelled);
        assert!(!d.join("big").exists());
        no_partials(&d);
    }

    #[test]
    fn moves_within_a_disk_and_undo_puts_them_back() {
        let s = Scratch::new();
        let (e, rx) = engine(&s);
        let f = s.file("src/a", b"1");
        s.file("src/d/inner", b"2");
        let d = s.dir("dest");
        e.start(Request::Move { sources: vec![f.clone(), s.path().join("src/d")], dest: d.clone() });
        let done = finish(&rx, vec![]);
        assert!(done.errors.is_empty());
        assert!(!f.exists());
        assert_eq!(fs::read(d.join("d/inner")).unwrap(), b"2");
        e.start(Request::Undo(done.outcome.undo.clone().unwrap()));
        let undone = finish(&rx, vec![]);
        assert!(undone.errors.is_empty(), "{:?}", undone.errors);
        assert_eq!(fs::read(&f).unwrap(), b"1");
        assert_eq!(fs::read(s.path().join("src/d/inner")).unwrap(), b"2");
        assert_eq!(fs::read_dir(&d).unwrap().count(), 0);
    }

    #[test]
    fn move_across_disks_copies_then_removes() {
        let s = Scratch::new();
        let (mut e, rx) = engine(&s);
        e.force_exdev = true;
        s.file("src/d/inner", b"2");
        s.file("src/d/sub/deep", b"3");
        symlink("inner", s.path().join("src/d/lnk")).unwrap();
        let d = s.dir("dest");
        e.start(Request::Move { sources: vec![s.path().join("src/d")], dest: d.clone() });
        let done = finish(&rx, vec![]);
        assert!(done.errors.is_empty(), "{:?}", done.errors);
        assert!(!s.path().join("src/d").exists());
        assert_eq!(fs::read(d.join("d/sub/deep")).unwrap(), b"3");
        assert_eq!(fs::read_link(d.join("d/lnk")).unwrap(), Path::new("inner"));
    }

    #[test]
    fn move_across_disks_lists_only_the_top_item_and_undoes_cleanly() {
        let s = Scratch::new();
        let (mut e, rx) = engine(&s);
        e.force_exdev = true;
        s.file("src/d/sub/deep", b"3");
        s.file("src/d/top", b"4");
        let d = s.dir("dest");
        e.start(Request::Move { sources: vec![s.path().join("src/d")], dest: d.clone() });
        let done = finish(&rx, vec![]);
        assert_eq!(done.outcome.created, vec![d.join("d")]);
        assert_eq!(done.outcome.undo, Some(Undo::Moves(vec![(s.path().join("src/d"), d.join("d"))])));
        e.start(Request::Undo(done.outcome.undo.unwrap()));
        let undone = finish(&rx, vec![]);
        assert!(undone.errors.is_empty(), "{:?}", undone.errors);
        assert_eq!(fs::read(s.path().join("src/d/sub/deep")).unwrap(), b"3");
        assert!(!d.join("d").exists());
    }

    #[test]
    fn keep_both_on_a_move_undoes_from_the_new_name() {
        let s = Scratch::new();
        let (e, rx) = engine(&s);
        let (src, dst) = conflict_setup(&s);
        e.start(Request::Move { sources: vec![src.clone()], dest: dst.parent().unwrap().into() });
        let done = finish(&rx, vec![answer(Choice::KeepBoth)]);
        assert_eq!(done.outcome.created, vec![s.path().join("dest/a (copy).txt")]);
        e.start(Request::Undo(done.outcome.undo.unwrap()));
        finish(&rx, vec![]);
        assert_eq!(fs::read(&src).unwrap(), b"new");
        assert_eq!(fs::read(&dst).unwrap(), b"old");
    }

    #[test]
    fn move_across_disks_keeps_sources_that_failed_to_copy() {
        let s = Scratch::new();
        let (mut e, rx) = engine(&s);
        e.force_exdev = true;
        let bad = s.file("src/d/bad", b"x");
        s.file("src/d/good", b"y");
        fs::set_permissions(&bad, fs::Permissions::from_mode(0o000)).unwrap();
        let d = s.dir("dest");
        e.start(Request::Move { sources: vec![s.path().join("src/d")], dest: d.clone() });
        let done = finish(&rx, vec![]);
        fs::set_permissions(&bad, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(done.errors.len(), 1, "{:?}", done.errors);
        assert_eq!(fs::read(&bad).unwrap(), b"x", "source that failed must stay");
        assert!(!s.path().join("src/d/good").exists());
        assert_eq!(fs::read(d.join("d/good")).unwrap(), b"y");
        assert_eq!(done.outcome.undo, None, "a partial move can't be undone as a whole");
    }

    #[test]
    fn move_conflict_replace_keeps_a_single_copy() {
        let s = Scratch::new();
        let (e, rx) = engine(&s);
        let (src, dst) = conflict_setup(&s);
        e.start(Request::Move { sources: vec![src.clone()], dest: dst.parent().unwrap().into() });
        finish(&rx, vec![answer(Choice::Replace)]);
        assert!(!src.exists());
        assert_eq!(fs::read(&dst).unwrap(), b"new");
    }

    #[test]
    fn delete_removes_a_tree_without_following_links() {
        let s = Scratch::new();
        let (e, rx) = engine(&s);
        let keep = s.file("keep/precious", b"p");
        s.file("gone/sub/x", b"x");
        symlink(s.path().join("keep"), s.path().join("gone/link")).unwrap();
        e.start(Request::Delete { paths: vec![s.path().join("gone")] });
        let done = finish(&rx, vec![]);
        assert!(done.errors.is_empty());
        assert!(!s.path().join("gone").exists());
        assert_eq!(fs::read(&keep).unwrap(), b"p");
    }

    #[test]
    fn delete_of_a_symlink_to_a_folder_keeps_the_target() {
        let s = Scratch::new();
        let (e, rx) = engine(&s);
        let keep = s.file("keep/precious", b"p");
        let link = s.path().join("lnk");
        symlink(s.path().join("keep"), &link).unwrap();
        e.start(Request::Delete { paths: vec![link.clone()] });
        finish(&rx, vec![]);
        assert!(fs::symlink_metadata(&link).is_err());
        assert!(keep.exists());
    }

    #[test]
    fn deleting_from_the_trash_takes_the_info_file_too() {
        let s = Scratch::new();
        let (e, rx) = engine(&s);
        let f = s.file("work/a", b"1");
        let t = trash::trash(&e.trash, &f).unwrap();
        e.start(Request::Delete { paths: vec![t.file.clone()] });
        finish(&rx, vec![]);
        assert!(!t.file.exists());
        assert!(!t.info.exists(), "the info file must not be orphaned");
    }

    #[test]
    fn trash_and_undo_through_the_engine() {
        let s = Scratch::new();
        let (e, rx) = engine(&s);
        let a = s.file("work/a", b"1");
        let b = s.file("work/b", b"2");
        e.start(Request::Trash { paths: vec![a.clone(), b.clone()] });
        let done = finish(&rx, vec![]);
        assert!(!a.exists() && !b.exists());
        assert!(s.path().join("data/Trash/files/a").exists());
        e.start(Request::Undo(done.outcome.undo.unwrap()));
        finish(&rx, vec![]);
        assert_eq!(fs::read(&a).unwrap(), b"1");
        assert_eq!(fs::read(&b).unwrap(), b"2");
        assert!(!s.path().join("data/Trash/files/a").exists());
    }

    #[test]
    fn trash_failure_is_reported_per_item() {
        let s = Scratch::new();
        let (e, rx) = engine(&s);
        let good = s.file("work/good", b"1");
        e.start(Request::Trash { paths: vec![s.path().join("work/missing"), good.clone()] });
        let done = finish(&rx, vec![]);
        assert_eq!(done.errors.len(), 1);
        assert!(!good.exists());
    }

    #[test]
    fn the_drop_rule_follows_the_keys_then_the_disks() {
        use DropVerb::*;
        for (ctrl, shift, same, can_delete, want) in [
            (false, false, Some(true), true, Move),
            (false, false, Some(false), true, Copy),
            (false, false, None, true, Copy),
            (false, false, Some(true), false, Copy),
            (true, false, Some(true), true, Copy),
            (false, true, Some(false), true, Move),
            (false, true, None, false, Move),
            (true, true, Some(true), true, Link),
            (true, true, None, false, Link),
        ] {
            assert_eq!(drop_verb(ctrl, shift, same, can_delete), want, "{ctrl} {shift} {same:?} {can_delete}");
        }
    }

    #[test]
    fn moving_something_onto_itself_counts_as_left_behind() {
        let s = Scratch::new();
        let (e, rx) = engine(&s);
        let here = s.file("work/here", b"1");
        let other = s.file("other/x", b"2");
        e.start(Request::Move { sources: vec![here.clone(), other], dest: s.path().join("work") });
        let done = finish(&rx, vec![]);
        assert!(done.errors.is_empty(), "{:?}", done.errors);
        assert_eq!(done.outcome.skipped, 1, "so a drop isn't reported as having all arrived");
        assert_eq!(fs::read(&here).unwrap(), b"1");
        assert!(s.path().join("work/x").exists());
    }

    #[test]
    fn links_point_at_the_source_and_leave_it_alone() {
        let s = Scratch::new();
        let (e, rx) = engine(&s);
        let f = s.file("src/a", b"1");
        let inner = s.file("src/d/inner", b"2");
        let d = s.dir("dest");
        e.start(Request::Link { sources: vec![f.clone(), s.path().join("src/d")], dest: d.clone() });
        let done = finish(&rx, vec![]);
        assert!(done.errors.is_empty(), "{:?}", done.errors);
        assert_eq!(fs::read_link(d.join("a")).unwrap(), fs::canonicalize(&f).unwrap());
        assert_eq!(fs::read(d.join("d/inner")).unwrap(), b"2");
        assert!(f.exists() && inner.exists());
        assert_eq!(done.outcome.created, vec![d.join("a"), d.join("d")]);
        assert_eq!(done.outcome.undo, None);
    }

    #[test]
    fn a_link_never_replaces_what_is_there() {
        let s = Scratch::new();
        let (e, rx) = engine(&s);
        let f = s.file("src/a", b"new");
        let kept = s.file("dest/a", b"old");
        let d = s.path().join("dest");
        e.start(Request::Link { sources: vec![f.clone()], dest: d.clone() });
        finish(&rx, vec![answer(Choice::Replace)]);
        assert_eq!(fs::read(&kept).unwrap(), b"old");
        assert!(!fs::symlink_metadata(&kept).unwrap().file_type().is_symlink());
        e.start(Request::Link { sources: vec![f.clone()], dest: d.clone() });
        let done = finish(&rx, vec![answer(Choice::KeepBoth)]);
        assert_eq!(fs::read(&kept).unwrap(), b"old");
        assert_eq!(done.outcome.created.len(), 1);
        assert_eq!(fs::read(&done.outcome.created[0]).unwrap(), b"new");
    }

    #[test]
    fn a_link_to_a_symlink_points_at_the_symlink() {
        let s = Scratch::new();
        let (e, rx) = engine(&s);
        let real = s.file("real", b"1");
        let lnk = s.path().join("lnk");
        symlink(&real, &lnk).unwrap();
        let d = s.dir("dest");
        e.start(Request::Link { sources: vec![lnk.clone()], dest: d.clone() });
        finish(&rx, vec![]);
        assert_eq!(fs::read_link(d.join("lnk")).unwrap(), fs::canonicalize(s.path()).unwrap().join("lnk"));
    }

    #[test]
    fn a_trash_batch_reads_the_mount_table_once_per_folder() {
        let s = Scratch::new();
        let (mut e, rx) = engine(&s);
        let lookups = Arc::new(AtomicU64::new(0));
        let counted = lookups.clone();
        e.trash.mount_of = Arc::new(move |_| {
            counted.fetch_add(1, Ordering::Relaxed);
            None
        });
        let paths = vec![s.file("work/a", b"1"), s.file("work/b", b"2"), s.file("work/c", b"3"), s.file("other/d", b"4")];
        e.start(Request::Trash { paths });
        let done = finish(&rx, vec![]);
        assert!(done.errors.is_empty(), "{:?}", done.errors);
        assert_eq!(lookups.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn a_failed_trash_sync_is_reported_once_and_the_items_stay_trashed() {
        let s = Scratch::new();
        let (mut e, rx) = engine(&s);
        e.force_sync_error = true;
        let a = s.file("work/a", b"1");
        let b = s.file("work/b", b"2");
        e.start(Request::Trash { paths: vec![a.clone(), b.clone()] });
        let done = finish(&rx, vec![]);
        assert_eq!(done.errors.len(), 1, "{:?}", done.errors);
        assert!(done.errors[0].1.contains("may not be saved"), "{:?}", done.errors);
        assert!(!a.exists() && !b.exists());
        assert!(s.path().join("data/Trash/files/a").exists() && s.path().join("data/Trash/info/a.trashinfo").exists());
        e.start(Request::Undo(done.outcome.undo.expect("the batch can still be undone")));
        finish(&rx, vec![]);
        assert_eq!(fs::read(&a).unwrap(), b"1");
    }

    /// More than one 8 MiB chunk, with every position different.
    fn patterned(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    #[test]
    fn sendfile_copies_from_the_current_position_to_the_end() {
        let s = Scratch::new();
        let data = patterned(9 * 1024 * 1024);
        let src = s.file("src", &data);
        let mut input = File::open(&src).unwrap();
        input.read_exact(&mut [0u8; 10]).unwrap();
        let out = s.path().join("out");
        let output = File::create(&out).unwrap();
        let mut reported = 0;
        let sent = sendfile_rest(&input, &output, |n| {
            reported += n;
            true
        })
        .unwrap();
        assert_eq!(sent, Sent::Complete(data.len() as u64 - 10));
        assert_eq!(reported, data.len() as u64 - 10);
        assert_eq!(fs::read(&out).unwrap(), &data[10..]);
    }

    #[test]
    fn sendfile_stops_when_cancelled() {
        let s = Scratch::new();
        let src = s.file("src", &patterned(9 * 1024 * 1024));
        let output = File::create(s.path().join("out")).unwrap();
        assert_eq!(sendfile_rest(&File::open(&src).unwrap(), &output, |_| false).unwrap(), Sent::Cancelled);
    }

    #[test]
    fn sendfile_reports_an_empty_input_as_nothing_sent() {
        let s = Scratch::new();
        let src = s.file("src", b"");
        let output = File::create(s.path().join("out")).unwrap();
        assert_eq!(sendfile_rest(&File::open(&src).unwrap(), &output, |_| true).unwrap(), Sent::Unsupported(0));
    }

    #[test]
    fn copies_to_another_filesystem_are_exact_with_and_without_sendfile() {
        let s = Scratch::new();
        let (mut e, rx) = engine(&s);
        let data = patterned(9 * 1024 * 1024 + 123);
        let f = s.file("big", &data);
        fs::set_permissions(&f, fs::Permissions::from_mode(0o640)).unwrap();
        let empty = s.file("empty", b"");
        let d = Path::new(TREE_WITH_MOUNT).join("mnt/copy-test");
        fs::create_dir_all(&d).unwrap();
        for no_sendfile in [false, true] {
            e.force_no_sendfile = no_sendfile;
            e.start(Request::Copy { sources: vec![f.clone(), empty.clone()], dest: d.clone() });
            let done = finish(&rx, vec![]);
            assert!(done.errors.is_empty(), "{:?}", done.errors);
            assert_eq!(fs::read(d.join("big")).unwrap(), data, "no_sendfile={no_sendfile}");
            assert_eq!(fs::metadata(d.join("big")).unwrap().permissions().mode() & 0o777, 0o640);
            assert_eq!(fs::read(d.join("empty")).unwrap(), b"");
            no_partials(&d);
            fs::remove_file(d.join("big")).unwrap();
            fs::remove_file(d.join("empty")).unwrap();
        }
        fs::remove_dir(&d).unwrap();
    }

    #[test]
    fn pseudo_files_that_report_no_size_still_copy() {
        let s = Scratch::new();
        let (e, rx) = engine(&s);
        let d = s.dir("dest");
        e.start(Request::Copy { sources: vec!["/proc/version".into()], dest: d.clone() });
        let done = finish(&rx, vec![]);
        assert!(done.errors.is_empty(), "{:?}", done.errors);
        let copied = fs::read(d.join("version")).unwrap();
        assert!(!copied.is_empty());
        assert_eq!(copied, fs::read("/proc/version").unwrap());
    }

    #[test]
    fn rename_refuses_to_overwrite_and_undo_works() {
        let s = Scratch::new();
        let (e, rx) = engine(&s);
        let a = s.file("a", b"1");
        let b = s.file("b", b"2");
        e.start(Request::Rename { from: a.clone(), to_name: "b".into() });
        let done = finish(&rx, vec![]);
        assert_eq!(done.errors.len(), 1);
        assert_eq!(fs::read(&b).unwrap(), b"2");
        e.start(Request::Rename { from: a.clone(), to_name: "c".into() });
        let done = finish(&rx, vec![]);
        assert_eq!(fs::read(s.path().join("c")).unwrap(), b"1");
        e.start(Request::Undo(done.outcome.undo.unwrap()));
        finish(&rx, vec![]);
        assert_eq!(fs::read(&a).unwrap(), b"1");
    }

    #[test]
    fn rename_rejects_bad_names_before_touching_anything() {
        let s = Scratch::new();
        let (e, rx) = engine(&s);
        let a = s.file("a", b"1");
        for bad in ["", ".", "..", "x/y", &"n".repeat(256)] {
            e.start(Request::Rename { from: a.clone(), to_name: bad.into() });
            let done = finish(&rx, vec![]);
            assert_eq!(done.errors.len(), 1, "{bad}");
            assert!(a.exists());
        }
    }

    #[test]
    fn new_folder_and_its_failures() {
        let s = Scratch::new();
        let (e, rx) = engine(&s);
        e.start(Request::NewFolder { parent: s.path().into(), name: "n".into() });
        let done = finish(&rx, vec![]);
        assert!(done.errors.is_empty());
        assert!(s.path().join("n").is_dir());
        assert_eq!(done.outcome.created, vec![s.path().join("n")]);
        e.start(Request::NewFolder { parent: s.path().into(), name: "n".into() });
        assert_eq!(finish(&rx, vec![]).errors.len(), 1);
        e.start(Request::NewFolder { parent: s.path().into(), name: "../escape".into() });
        assert_eq!(finish(&rx, vec![]).errors.len(), 1);
        assert!(!s.path().parent().unwrap().join("escape").exists());
    }

    #[test]
    fn a_failed_sync_keeps_the_source_of_a_cross_disk_move() {
        let s = Scratch::new();
        let (mut e, rx) = engine(&s);
        e.force_exdev = true;
        e.force_sync_error = true;
        let f = s.file("src/a", b"only copy");
        let d = s.dir("dest");
        e.start(Request::Move { sources: vec![f.clone()], dest: d.clone() });
        let done = finish(&rx, vec![]);
        assert_eq!(done.errors.len(), 1, "{:?}", done.errors);
        assert_eq!(fs::read(&f).unwrap(), b"only copy");
        assert_eq!(done.outcome.undo, None);
        no_partials(&d);
    }

    /// Scripts/sandbox-run.sh mounts a tmpfs at this path, inside bubblewrap.
    const TREE_WITH_MOUNT: &str = "/run/gannet-tree";

    #[test]
    fn delete_stops_at_a_mount_point_inside_the_folder() {
        let s = Scratch::new();
        let (e, rx) = engine(&s);
        let tree = Path::new(TREE_WITH_MOUNT);
        let precious = tree.join("mnt/precious");
        fs::write(&precious, b"on another disk").unwrap();
        fs::write(tree.join("ordinary"), b"x").unwrap();
        e.start(Request::Delete { paths: vec![tree.to_path_buf()] });
        let done = finish(&rx, vec![]);
        assert_eq!(fs::read(&precious).unwrap(), b"on another disk");
        fs::remove_file(&precious).unwrap();
        assert!(!tree.join("ordinary").exists());
        assert_eq!(done.errors.len(), 1, "{:?}", done.errors);
        assert!(done.errors[0].1.contains("mount point"));
    }

    #[test]
    fn apply_to_all_on_a_folder_merge_doesnt_overwrite_files() {
        let s = Scratch::new();
        let (e, rx) = engine(&s);
        s.file("src/d/f", b"incoming");
        s.file("dest/d/f", b"precious");
        e.start(Request::Copy { sources: vec![s.path().join("src/d")], dest: s.path().join("dest") });
        finish(&rx, vec![all(Choice::Replace), answer(Choice::Skip)]);
        assert_eq!(fs::read(s.path().join("dest/d/f")).unwrap(), b"precious");
    }

    #[test]
    fn a_merged_move_has_no_undo_and_keeps_what_was_there() {
        let s = Scratch::new();
        let (e, rx) = engine(&s);
        s.file("src/d/new", b"n");
        s.file("dest/d/preexisting", b"p");
        e.start(Request::Move { sources: vec![s.path().join("src/d")], dest: s.path().join("dest") });
        let done = finish(&rx, vec![answer(Choice::Replace)]);
        assert_eq!(done.outcome.undo, None);
        assert_eq!(fs::read(s.path().join("dest/d/preexisting")).unwrap(), b"p");
        assert_eq!(fs::read(s.path().join("dest/d/new")).unwrap(), b"n");
    }

    #[test]
    fn a_trailing_slash_on_a_symlink_doesnt_follow_it() {
        let s = Scratch::new();
        let (e, rx) = engine(&s);
        let kept = s.file("real/kept", b"k");
        let link = s.path().join("lnk");
        symlink(s.path().join("real"), &link).unwrap();
        let mut slashed = link.clone().into_os_string();
        slashed.push("/");
        e.start(Request::Delete { paths: vec![PathBuf::from(slashed)] });
        let done = finish(&rx, vec![]);
        assert!(done.errors.is_empty(), "{:?}", done.errors);
        assert_eq!(fs::read(&kept).unwrap(), b"k");
        assert!(fs::symlink_metadata(&link).is_err());
    }

    #[test]
    fn copies_drop_setuid_and_setgid() {
        let s = Scratch::new();
        let (e, rx) = engine(&s);
        let f = s.file("a", b"x");
        fs::set_permissions(&f, fs::Permissions::from_mode(0o6755)).unwrap();
        let d = s.dir("dest");
        e.start(Request::Copy { sources: vec![f], dest: d.clone() });
        finish(&rx, vec![]);
        assert_eq!(fs::metadata(d.join("a")).unwrap().permissions().mode() & 0o7777, 0o755);
    }

    #[test]
    fn extended_attributes_are_copied() {
        let s = Scratch::new();
        let (e, rx) = engine(&s);
        let f = s.file("a", b"x");
        let c = std::ffi::CString::new(f.as_os_str().as_bytes()).unwrap();
        // SAFETY: valid C strings and a valid buffer of the given length.
        let set = unsafe { libc::setxattr(c.as_ptr(), c"user.gannet".as_ptr(), b"v".as_ptr().cast(), 1, 0) };
        assert_eq!(set, 0, "the sandbox's filesystem must support user xattrs");
        let d = s.dir("dest");
        e.start(Request::Copy { sources: vec![f], dest: d.clone() });
        finish(&rx, vec![]);
        let c = std::ffi::CString::new(d.join("a").as_os_str().as_bytes()).unwrap();
        let mut buf = [0u8; 8];
        // SAFETY: as above.
        let got = unsafe { libc::getxattr(c.as_ptr(), c"user.gannet".as_ptr(), buf.as_mut_ptr().cast(), buf.len()) };
        assert_eq!(got, 1);
        assert_eq!(buf[0], b'v');
    }

    #[test]
    fn cancel_ends_a_job_waiting_on_a_conflict() {
        let s = Scratch::new();
        let (e, rx) = engine(&s);
        let (src, dst) = conflict_setup(&s);
        let job = e.start(Request::Copy { sources: vec![src], dest: dst.parent().unwrap().into() });
        let mut held = None;
        loop {
            match rx.recv_timeout(Duration::from_secs(5)).unwrap() {
                Event::Conflict(c) => held = Some(c),
                Event::Finished(o) => {
                    assert!(o.cancelled);
                    break;
                }
                _ => {}
            }
            if held.is_some() {
                job.cancel();
            }
        }
        assert_eq!(fs::read(&dst).unwrap(), b"old");
    }

    #[test]
    fn hard_links_are_a_conflict_not_the_same_place() {
        let s = Scratch::new();
        let (e, rx) = engine(&s);
        let a = s.file("src/a", b"x");
        let d = s.dir("dest");
        fs::hard_link(&a, d.join("a")).unwrap();
        e.start(Request::Move { sources: vec![a.clone()], dest: d.clone() });
        let done = finish(&rx, vec![answer(Choice::Replace)]);
        assert!(done.errors.is_empty());
        assert!(!a.exists());
        assert!(d.join("a").exists());
    }

    #[test]
    fn an_unreadable_folder_leaves_no_empty_copy() {
        let s = Scratch::new();
        let (e, rx) = engine(&s);
        let src = s.dir("src");
        fs::set_permissions(&src, fs::Permissions::from_mode(0o000)).unwrap();
        let d = s.dir("dest");
        e.start(Request::Copy { sources: vec![src.clone()], dest: d.clone() });
        let done = finish(&rx, vec![]);
        fs::set_permissions(&src, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(done.errors.len(), 1);
        assert!(!d.join("src").exists());
    }

    #[test]
    fn progress_isnt_inflated_by_a_cross_disk_folder_move() {
        let s = Scratch::new();
        let (mut e, rx) = engine(&s);
        e.force_exdev = true;
        s.file("src/a/b/c/f", &[0; 1000]);
        let job = e.start(Request::Move { sources: vec![s.path().join("src/a")], dest: s.dir("dest") });
        finish(&rx, vec![]);
        assert_eq!(job.progress.total_bytes.load(Ordering::Relaxed), 1000);
        assert_eq!(job.progress.done_bytes.load(Ordering::Relaxed), 1000);
    }

    #[test]
    fn undo_reports_only_what_it_restored() {
        let s = Scratch::new();
        let (e, rx) = engine(&s);
        let f = s.file("src/a", b"1");
        let d = s.dir("dest");
        e.start(Request::Move { sources: vec![f.clone()], dest: d.clone() });
        let done = finish(&rx, vec![]);
        fs::write(&f, b"took its place").unwrap();
        e.start(Request::Undo(done.outcome.undo.unwrap()));
        let undone = finish(&rx, vec![answer(Choice::Skip)]);
        assert!(undone.outcome.created.is_empty(), "{:?}", undone.outcome.created);
        assert_eq!(fs::read(&f).unwrap(), b"took its place");
        assert_eq!(fs::read(d.join("a")).unwrap(), b"1");
    }

    #[test]
    fn long_copy_names_dont_cut_a_character_in_half() {
        let s = Scratch::new();
        // 255 bytes, with the cut for " (copy)" landing inside a character.
        let name = format!("a{}", "é".repeat(127));
        let f = s.file(&name, b"");
        let n = unique_copy_name(&f);
        assert!(n.file_name().unwrap().to_str().is_some(), "still valid UTF-8");
    }

    #[test]
    fn copy_names_keep_extensions_except_folders_and_dotfiles() {
        let s = Scratch::new();
        let f = s.file("archive.tar.gz", b"");
        assert_eq!(unique_copy_name(&f).file_name().unwrap(), "archive.tar (copy).gz");
        let d = s.dir("v1.2");
        assert_eq!(unique_copy_name(&d).file_name().unwrap(), "v1.2 (copy)");
        let dot = s.file(".bashrc", b"");
        assert_eq!(unique_copy_name(&dot).file_name().unwrap(), ".bashrc (copy)");
        let long = s.file(&"n".repeat(255), b"");
        assert!(unique_copy_name(&long).file_name().unwrap().len() <= 255);
    }

    #[test]
    fn progress_totals_add_up() {
        let s = Scratch::new();
        let (e, rx) = engine(&s);
        s.file("src/a", &[0; 1000]);
        s.file("src/b", &[0; 2000]);
        let job = e.start(Request::Copy { sources: vec![s.path().join("src")], dest: s.dir("dest") });
        finish(&rx, vec![]);
        assert_eq!(job.progress.total_bytes.load(Ordering::Relaxed), 3000);
        assert_eq!(job.progress.done_bytes.load(Ordering::Relaxed), 3000);
        assert_eq!(job.progress.total_items.load(Ordering::Relaxed), 3);
        assert_eq!(job.progress.done_items.load(Ordering::Relaxed), 3);
    }
}
