//! The preview: what's showing, the background work that fetches it, and drawing it. It runs in a
//! window of its own, in a process of its own (`gannet --preview`), which is an `App` with
//! `viewer` set: its folder listing is what the arrows walk through. The file manager's side of
//! it is at the end: opening and closing that process, and following each other's cursor.
//! Nothing here decodes. That happens in yet another process (see `decode`), and this thread
//! only ever draws pixels that are ready.

use std::fs::File;
use std::io::Read;
use std::os::fd::OwnedFd;
use std::os::unix::fs::{FileExt, FileTypeExt, MetadataExt, OpenOptionsExt};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use gannet_core::history::Stop;
use gannet_core::listing::Stat;
use gannet_core::mime::{self, MimeDb};
use gannet_core::thumbcache;
use rustix::event::{EventfdFlags, PollFd, PollFlags, Timespec, eventfd, poll};
use rustix::fs::{MemfdFlags, SealFlags, fcntl_add_seals, fstat, memfd_create};
use rustix::mm::{MapFlags, ProtFlags, mmap, munmap};
use rustix::pipe::{PipeFlags, pipe_with};
use xkbcommon::xkb::keysyms as k;

use crate::app::{App, BTN_EXTRA, BTN_LEFT, BTN_SIDE, Cmd, Hit, Hub, Msg};
use crate::canvas::{Canvas, Rect};
use crate::decode;
use crate::format;
use crate::icons::{self, Icon};
use crate::media::{self, Ending, Frames, PlayState, Remote, Said, StartError};
use crate::wl::Mods;

/// A file arriving this fast never shows its thumbnail, or a flash of one.
const THUMBNAIL_AFTER: Duration = Duration::from_millis(30);
/// Words on screen for a blink look like a glitch, so they wait.
const LOADING_AFTER: Duration = Duration::from_millis(120);
/// Holding an arrow runs through files. Only stopping on one is worth a process.
const MOVE_SETTLE: Duration = Duration::from_millis(40);
const THUMBNAIL_LIMIT: Duration = Duration::from_secs(2);
/// Longer than the decoder's CPU limit, for a file on a stalled disk.
const DECODE_LIMIT: Duration = Duration::from_secs(30);
/// A smooth copy this many times the popup's own size is mostly memory nobody can see.
const SMOOTH_MAX_AREAS: i64 = 4;
const ZOOM_STEP: f64 = 1.25;
const ZOOM_MAX: f64 = 16.0;
const REPORTS_CATCH_UP: Duration = Duration::from_millis(250);
const SEEK_STEP: f64 = 5.0;
const VOLUME_STEP: f64 = 5.0;
const TITLE_H: f64 = 38.0;
const FOOT_H: f64 = 32.0;

const DAMAGED: &str = "This image can't be read. It may be damaged.";
const TOO_LARGE: &str = "This image is too large to preview";
const NO_BUBBLEWRAP: &str = "Video previews need bubblewrap, which isn't installed";
const NO_MPV: &str = "Video previews need mpv, which isn't installed";
const PLAYER_FAILED: &str = "The video player couldn't start";
const NO_JAIL: &str = "Video previews couldn't start their sandbox";
const CANT_PLAY: &str = "This video can't be played. It may be damaged.";
const SANDBOX_STOPPED: &str = "The preview was stopped by its sandbox";
const NO_SANDBOX: &str = "Previews are off: this system can't run the previewer safely";

/// Pixels in memory another process wrote, so dropping this hands them straight back.
pub struct ImageBuf {
    ptr: *mut std::ffi::c_void,
    len: usize,
    pub w: u32,
    pub h: u32,
}

// SAFETY: the mapping is read-only and sealed against shrinking, and only `Drop` unmaps it.
unsafe impl Send for ImageBuf {}
// SAFETY: as above; shared reads of it are fine.
unsafe impl Sync for ImageBuf {}

impl ImageBuf {
    /// Checks what the decoder wrote, as it may have been handed a hostile file.
    fn from_memfd(fd: &OwnedFd) -> Option<Self> {
        // Without this, a truncated file would fault when read rather than fail here.
        fcntl_add_seals(fd, SealFlags::SHRINK | SealFlags::GROW).ok()?;
        let len = usize::try_from(fstat(fd).ok()?.st_size).ok()?;
        if len < decode::HEADER {
            return None;
        }
        // SAFETY: a new read-only mapping of a sealed file, at an address the kernel chooses.
        let ptr = unsafe { mmap(std::ptr::null_mut(), len, ProtFlags::READ, MapFlags::SHARED, fd, 0) }.ok()?;
        let mut image = Self { ptr, len, w: 0, h: 0 };
        // SAFETY: `len` bytes are mapped, and at least `HEADER` of them exist.
        let header = unsafe { std::slice::from_raw_parts(ptr as *const u8, decode::HEADER) };
        if &header[..4] != decode::MAGIC {
            return None;
        }
        image.w = u32::from_ne_bytes(header[4..8].try_into().ok()?);
        image.h = u32::from_ne_bytes(header[8..12].try_into().ok()?);
        let want = (image.w as u64).checked_mul(image.h as u64)?.checked_mul(4)?.checked_add(decode::HEADER as u64)?;
        (image.w > 0 && image.h > 0 && want == len as u64).then_some(image)
    }

    pub fn pixels(&self) -> &[u8] {
        // SAFETY: mapped for `len` bytes until drop, and `from_memfd` checked the header fits.
        unsafe { std::slice::from_raw_parts((self.ptr as *const u8).add(decode::HEADER), self.len - decode::HEADER) }
    }
}

impl Drop for ImageBuf {
    fn drop(&mut self) {
        // SAFETY: this is the mapping from `from_memfd`, and nothing can still borrow it.
        let _ = unsafe { munmap(self.ptr, self.len) };
    }
}

/// The file-type database, read the first time a preview needs it rather than at startup.
pub struct Kinds {
    dirs: Vec<PathBuf>,
    lang: Option<String>,
    db: OnceLock<MimeDb>,
}

impl Kinds {
    /// `dirs` are the `mime` folders, first wins; `lang` is the user's language, like `de_DE.UTF-8`.
    pub fn new(dirs: Vec<PathBuf>, lang: Option<String>) -> Self {
        Self { dirs, lang, db: OnceLock::new() }
    }

    fn db(&self) -> &MimeDb {
        self.db.get_or_init(|| MimeDb::load(&self.dirs))
    }
}

impl Default for Kinds {
    fn default() -> Self {
        Self::new(Vec::new(), None)
    }
}

/// What can be said about any file, for the ones that have nothing better to show.
pub struct Info {
    pub mime: String,
    pub description: String,
    pub icon: Icon,
    pub size: Option<u64>,
    pub modified: Option<i64>,
    /// A picture another program already made of it, if there is one.
    pub thumbnail: Option<Arc<ImageBuf>>,
}

pub enum Found {
    Thumbnail(Arc<ImageBuf>),
    Info(Info),
    Full { image: Arc<ImageBuf>, size: u64 },
    Message(String),
    Scaled { scale_gen: u64, w: i32, h: i32, rgba: Vec<u8> },
    VideoStarted { remote: Remote, frames: Frames },
    VideoFrame { index: usize, w: u32, h: u32 },
    VideoState(PlayState),
}

pub struct Event {
    pub generation: u64,
    pub found: Found,
}

/// One running decoder process.
struct Helper {
    child: Child,
    /// Its stderr. Nothing's read from this but the end of it, which means it has exited.
    gone: OwnedFd,
    output: OwnedFd,
    status: Option<ExitStatus>,
    /// The pipe has ended. That nearly always means it has exited, but a process can close its
    /// stderr and carry on, so it still gets asked rather than waited on.
    pipe_ended: bool,
}

enum Waited {
    Exited(ExitStatus),
    Cancelled,
    TimedOut,
}

impl Helper {
    fn spawn(input: File) -> std::io::Result<Self> {
        let output = memfd_create("gannet-preview", MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING)?;
        // `/proc/self/exe` rather than a path: this binary, even if it's been upgraded since.
        let mut command = Command::new("/proc/self/exe");
        command.arg(decode::ARG).env_clear().current_dir("/").stdin(Stdio::from(input));
        Self::run(command, output)
    }

    fn run(mut command: Command, output: OwnedFd) -> std::io::Result<Self> {
        let (gone, stderr) = pipe_with(PipeFlags::CLOEXEC)?;
        let child = command.stdout(Stdio::from(output.try_clone()?)).stderr(Stdio::from(stderr)).spawn()?;
        // The `Command` holds its copies of these until it's dropped, and the end of the pipe
        // can't show until then.
        drop(command);
        Ok(Self { child, gone, output, status: None, pipe_ended: false })
    }

    fn wait(&mut self, cancel: &OwnedFd, limit: Duration) -> Waited {
        if let Some(status) = self.status {
            return Waited::Exited(status);
        }
        let deadline = Instant::now() + limit;
        loop {
            if self.pipe_ended {
                match self.child.try_wait() {
                    Ok(Some(status)) => {
                        self.status = Some(status);
                        return Waited::Exited(status);
                    }
                    Ok(None) => {}
                    Err(_) => return Waited::TimedOut,
                }
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Waited::TimedOut;
            }
            let wait = if self.pipe_ended { left.min(Duration::from_millis(1)) } else { left };
            let timeout = Timespec { tv_sec: wait.as_secs() as _, tv_nsec: wait.subsec_nanos() as _ };
            let mut fds = [PollFd::new(cancel, PollFlags::IN), PollFd::new(&self.gone, PollFlags::IN)];
            let watching = if self.pipe_ended { 1 } else { 2 };
            match poll(&mut fds[..watching], Some(&timeout)) {
                Ok(_) | Err(rustix::io::Errno::INTR) => {}
                Err(_) => return Waited::TimedOut,
            }
            let (cancelled, readable) = (!fds[0].revents().is_empty(), !fds[1].revents().is_empty());
            if cancelled {
                return Waited::Cancelled;
            }
            if readable && !self.pipe_ended {
                let mut buf = [0u8; 256];
                self.pipe_ended = matches!(rustix::io::read(&self.gone, &mut buf), Ok(0) | Err(_));
            }
        }
    }
}

impl Drop for Helper {
    fn drop(&mut self) {
        if self.status.is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// The background work for one file. Dropping this stops it, which kills its decoder.
pub struct Job {
    cancel: Arc<OwnedFd>,
}

impl Job {
    fn start(hub: Hub, generation: u64, path: PathBuf, kinds: Arc<Kinds>, cache_home: Option<PathBuf>, slow: Option<Duration>, video_size: (i32, i32)) -> Option<Self> {
        let cancel = Arc::new(eventfd(0, EventfdFlags::CLOEXEC | EventfdFlags::NONBLOCK).ok()?);
        let theirs = cancel.clone();
        std::thread::Builder::new()
            .name("gannet-preview".into())
            .spawn(move || {
                let place = Place { path: &path, kinds: &kinds, cache_home: cache_home.as_deref() };
                if let Some(found) = fetch(&place, slow, video_size, &theirs, &|found| hub.send(Msg::Preview(Event { generation, found }))) {
                    hub.send(Msg::Preview(Event { generation, found }));
                }
            })
            .ok()?;
        Some(Self { cancel })
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        let _ = rustix::io::write(&*self.cancel, &1u64.to_ne_bytes());
    }
}

fn message(text: &str) -> Option<Found> {
    Some(Found::Message(text.to_string()))
}

/// The file being fetched, and what's needed to describe it.
struct Place<'a> {
    path: &'a Path,
    kinds: &'a Kinds,
    cache_home: Option<&'a Path>,
}

/// Gets one file on screen. Returns its final answer, or `None` if it's been cancelled.
fn fetch(at: &Place, slow: Option<Duration>, video_size: (i32, i32), cancel: &OwnedFd, send: &dyn Fn(Found)) -> Option<Found> {
    let path = at.path;
    // Non-blocking so a FIFO can't hang this on open. It's turned away just below.
    let file = match File::options().read(true).custom_flags(libc::O_NONBLOCK).open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            return message("You don't have permission to read this file");
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return message("This file is no longer here");
        }
        Err(_) => return message("This file can't be read"),
    };
    let Ok(meta) = file.metadata() else { return message("This file can't be read") };
    if !meta.is_file() {
        return Some(describe(at, &meta, None, cancel));
    }
    // Read where it is rather than with `read`, which would move the decoder's starting point.
    let mut head = vec![0u8; mime::HEAD];
    let head = file.read_at(&mut head, 0).ok().map(|n| head[..n].to_vec());
    let mut helper = match Helper::spawn(file) {
        Ok(h) => h,
        Err(_) => return message("The previewer couldn't start"),
    };
    let mut waited = helper.wait(cancel, THUMBNAIL_AFTER);
    if slow.is_some() || matches!(waited, Waited::TimedOut) {
        if let Some(cache_home) = at.cache_home
            && let Some(image) = thumbnail(cache_home, path, meta.mtime(), cancel)
        {
            send(Found::Thumbnail(Arc::new(image)));
        }
        if let Some(slow) = slow
            && cancelled_within(cancel, slow)
        {
            return None;
        }
        waited = helper.wait(cancel, DECODE_LIMIT);
    }
    let unsupported = || Some(describe(at, &meta, head.as_deref(), cancel));
    match waited {
        Waited::Cancelled => None,
        Waited::TimedOut => message("This file is taking too long to read"),
        Waited::Exited(status) if status.code() == Some(decode::EXIT_UNSUPPORTED) && head.as_deref().is_some_and(media::looks_like_video) => play_video(path, video_size, cancel, send, &|| describe(at, &meta, head.as_deref(), cancel)),
        Waited::Exited(status) => answer(status, &helper.output, meta.len()).or_else(unsupported),
    }
}

/// The type, name, size and date of anything, and a picture of it if another program made one.
fn describe(at: &Place, meta: &std::fs::Metadata, head: Option<&[u8]>, cancel: &OwnedFd) -> Found {
    let name = at.path.file_name().map_or(&b""[..], |n| n.as_encoded_bytes());
    let kind = meta.file_type();
    let db = at.kinds.db();
    let mime = if kind.is_dir() {
        mime::FOLDER
    } else if kind.is_fifo() {
        "inode/fifo"
    } else if kind.is_socket() {
        "inode/socket"
    } else if kind.is_char_device() {
        "inode/chardevice"
    } else if kind.is_block_device() {
        "inode/blockdevice"
    } else {
        db.guess(name, head)
    };
    let description = db.describe(mime, at.kinds.lang.as_deref()).unwrap_or_else(|| fallback_description(mime));
    let icon = icon_for(mime, &db.generic_icon(mime), name);
    let thumbnail = match at.cache_home {
        Some(cache_home) if kind.is_file() => thumbnail(cache_home, at.path, meta.mtime(), cancel).map(Arc::new),
        _ => None,
    };
    let size = kind.is_file().then_some(meta.len());
    Found::Info(Info { mime: mime.to_string(), description, icon, size, modified: Some(meta.mtime()), thumbnail })
}

/// Words for a type the database doesn't name, which is what an empty database gives.
fn fallback_description(mime: &str) -> String {
    match mime {
        mime::FOLDER => "Folder",
        "inode/fifo" => "Pipe",
        "inode/socket" => "Socket",
        "inode/chardevice" | "inode/blockdevice" => "Device",
        mime::PLAIN_TEXT => "Plain text document",
        mime::EMPTY => "Empty file",
        _ => return mime.to_string(),
    }
    .to_string()
}

/// One of Gannet's own icons for a type, by the generic icon the database gives it.
fn icon_for(mime: &str, generic: &str, name: &[u8]) -> Icon {
    if mime == mime::FOLDER {
        return Icon::Folder;
    }
    match generic {
        "image-x-generic" => Icon::Image,
        "video-x-generic" => Icon::Video,
        "audio-x-generic" => Icon::Audio,
        "package-x-generic" => Icon::Archive,
        "text-x-script" | "text-html" => Icon::Code,
        "text-x-generic" | "x-office-document" | "x-office-spreadsheet" | "x-office-presentation" | "x-office-document-template" => Icon::Document,
        _ => match icons::icon_for_name(name) {
            Icon::File if mime.starts_with("text/") => Icon::Document,
            other => other,
        },
    }
}

/// Plays a video until it's cancelled or something stops it. Returns the words to show if so.
/// A file that turns out to have no picture gets `otherwise` instead.
fn play_video(path: &Path, size: (i32, i32), cancel: &OwnedFd, send: &dyn Fn(Found), otherwise: &dyn Fn() -> Found) -> Option<Found> {
    let Ok(video) = File::options().read(true).custom_flags(libc::O_NONBLOCK).open(path) else { return message("This file can't be read") };
    // It may have been swapped for something else since it was first looked at.
    if !video.metadata().is_ok_and(|m| m.is_file()) {
        return message("This file has changed. Look at it again to see what it is now.");
    }
    let started = match media::start_jailed(video, size, &[media::ARG]) {
        Ok(started) => started,
        Err(StartError::NoBubblewrap) => return message(NO_BUBBLEWRAP),
        Err(StartError::Other) => return message(NO_JAIL),
    };
    send(Found::VideoStarted { remote: started.remote, frames: started.frames });
    let mut hello = false;
    let status = media::watch(started.child, started.said, cancel, |said| match said {
        Said::Hello => hello = true,
        Said::Frame { index, w, h } => send(Found::VideoFrame { index, w, h }),
        Said::State(state) => send(Found::VideoState(state)),
        // Showing something else drops this job, and that stops the player.
        Said::NoVideo => send(otherwise()),
        Said::CantPlay => send(Found::Message(CANT_PLAY.to_string())),
    })?;
    match media::ending(status, hello) {
        Ending::NoMpv => message(NO_MPV),
        Ending::NoSandbox => message(NO_SANDBOX),
        Ending::PlayerFailed => message(PLAYER_FAILED),
        Ending::JailFailed => message(NO_JAIL),
        Ending::SandboxStopped => {
            eprintln!("gannet: the sandbox stopped a video player");
            message(SANDBOX_STOPPED)
        }
        Ending::Other => message(CANT_PLAY),
    }
}

/// What the decoder's ending means. `None` when it doesn't know the format, so something else
/// should say what the file is.
fn answer(status: ExitStatus, output: &OwnedFd, size: u64) -> Option<Found> {
    match status.code() {
        Some(0) => match ImageBuf::from_memfd(output) {
            Some(image) => Some(Found::Full { image: Arc::new(image), size }),
            None => message(DAMAGED),
        },
        Some(decode::EXIT_UNSUPPORTED) => None,
        Some(decode::EXIT_TOO_LARGE) => message(TOO_LARGE),
        Some(decode::EXIT_NO_SANDBOX) => message(NO_SANDBOX),
        // Out of memory or CPU, which is what a limit looks like from out here. The kernel sends
        // SIGKILL for the CPU limit, as its soft and hard limits are equal. It can't be our own
        // kill: this only runs for a process we were still waiting for.
        None if matches!(status.signal(), Some(libc::SIGABRT | libc::SIGKILL | libc::SIGXFSZ)) => message(TOO_LARGE),
        // A hostile file, or something a decoder really needs that the sandbox doesn't allow. It
        // mustn't read as a bad file, or a gap in the sandbox would look like damaged images.
        None if status.signal() == Some(libc::SIGSYS) => {
            eprintln!("gannet: the sandbox stopped a preview decoder");
            message(SANDBOX_STOPPED)
        }
        _ => message(DAMAGED),
    }
}

fn cancelled_within(cancel: &OwnedFd, limit: Duration) -> bool {
    let timeout = Timespec { tv_sec: limit.as_secs() as _, tv_nsec: limit.subsec_nanos() as _ };
    let mut fds = [PollFd::new(cancel, PollFlags::IN)];
    let _ = poll(&mut fds, Some(&timeout));
    !fds[0].revents().is_empty()
}

/// Something other programs already made, if it's there and of this version of the file.
fn thumbnail(cache_home: &Path, path: &Path, mtime: i64, cancel: &OwnedFd) -> Option<ImageBuf> {
    for candidate in thumbcache::candidates(cache_home, path) {
        let Ok(mut file) = File::options().read(true).custom_flags(libc::O_NONBLOCK).open(&candidate) else { continue };
        if !file.metadata().is_ok_and(|m| m.is_file() && m.len() <= thumbcache::MAX_THUMBNAIL) {
            continue;
        }
        let mut bytes = Vec::new();
        if file.read_to_end(&mut bytes).is_err() || thumbcache::recorded_mtime(&bytes) != Some(mtime) {
            continue;
        }
        let Ok(input) = File::open(&candidate) else { continue };
        let Ok(mut helper) = Helper::spawn(input) else { continue };
        match helper.wait(cancel, THUMBNAIL_LIMIT) {
            Waited::Cancelled => return None,
            Waited::Exited(status) if status.success() => {
                if let Some(image) = ImageBuf::from_memfd(&helper.output) {
                    return Some(image);
                }
            }
            _ => {}
        }
    }
    None
}

/// Shrinks by averaging, weighted by alpha so transparent pixels don't bleed their color.
/// Comes back straight, not premultiplied. `None` if cancelled.
pub fn box_scale(src: &[u8], sw: usize, sh: usize, dw: usize, dh: usize, cancel: &AtomicBool) -> Option<Vec<u8>> {
    let mut out = vec![0u8; dw * dh * 4];
    for (y, row) in out.chunks_mut(dw * 4).enumerate() {
        if cancel.load(Ordering::Relaxed) {
            return None;
        }
        let y0 = y * sh / dh;
        let y1 = ((y + 1) * sh / dh).max(y0 + 1);
        for (x, px) in row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let x0 = x * sw / dw;
            let x1 = ((x + 1) * sw / dw).max(x0 + 1);
            let (mut r, mut g, mut b, mut a, mut n) = (0u64, 0u64, 0u64, 0u64, 0u64);
            for line in src[y0 * sw * 4..y1 * sw * 4].chunks_exact(sw * 4) {
                for p in line[x0 * 4..x1 * 4].as_chunks::<4>().0 {
                    let alpha = p[3] as u64;
                    r += p[0] as u64 * alpha;
                    g += p[1] as u64 * alpha;
                    b += p[2] as u64 * alpha;
                    a += alpha;
                    n += 1;
                }
            }
            px[0] = r.checked_div(a).unwrap_or(0) as u8;
            px[1] = g.checked_div(a).unwrap_or(0) as u8;
            px[2] = b.checked_div(a).unwrap_or(0) as u8;
            px[3] = (a / n) as u8;
        }
    }
    Some(out)
}

pub enum Content {
    Waiting,
    Image { image: Arc<ImageBuf>, thumbnail: bool },
    Info(Info),
    Video(Video),
    Message(String),
}

/// A running player, and what it last said.
pub struct Video {
    remote: Remote,
    frames: Frames,
    pub frame: Option<(usize, u32, u32)>,
    pub state: PlayState,
    pub frames_shown: u64,
    /// What was asked for stands until then, as reports on their way may be from before it.
    settled_at: Instant,
}

#[derive(Clone, Copy, PartialEq)]
enum Scrub {
    Seek,
    Volume,
}

/// Where the video controls go, in the footer.
struct Controls {
    play: Rect,
    time_x: i32,
    seek: Rect,
    mute: Rect,
    volume: Rect,
}

fn fraction_along(track: Rect, x: i32) -> f64 {
    if track.w <= 0 { 0.0 } else { ((x - track.x) as f64 / track.w as f64).clamp(0.0, 1.0) }
}

fn clock(seconds: f64) -> String {
    let total = seconds.max(0.0) as u64;
    let (h, m, s) = (total / 3600, total / 60 % 60, total % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") }
}

struct Scaled {
    w: i32,
    h: i32,
    rgba: Vec<u8>,
}

pub struct Preview {
    generation: u64,
    pub path: PathBuf,
    pub content: Content,
    pub size: Option<u64>,
    /// Image pixels to window pixels. `None` fits the image to the popup.
    pub zoom: Option<f64>,
    pub pan: (f64, f64),
    pub loading_shown: bool,
    /// Whether a popup was already up, so there's no gap in it while the next file loads.
    continuing: bool,
    /// The size and time the listing has for this file, to notice it being overwritten.
    seen: Option<(u64, i64)>,
    scaled: Option<Scaled>,
    scale_gen: u64,
    scale_wanted: (i32, i32),
    scale_cancel: Arc<AtomicBool>,
    job: Option<Job>,
    start_at: Option<Instant>,
    loading_at: Option<Instant>,
    drag: Option<(i32, i32, f64, f64)>,
    scrub: Option<Scrub>,
}

impl Drop for Preview {
    fn drop(&mut self) {
        self.scale_cancel.store(true, Ordering::Relaxed);
    }
}

impl Preview {
    #[cfg(feature = "bench")]
    pub fn has_scaled(&self) -> bool {
        self.scaled.is_some()
    }
}

/// Where an image goes in the popup.
struct View {
    area: Rect,
    dest: Rect,
    scale: f64,
    fit: f64,
    pan: (f64, f64),
}

/// The scale for an image in this space, and the scale that fits it. A small image stays sharp
/// at its own size. A thumbnail is standing in for a bigger one, so it fills the space.
fn image_scale(area: (i32, i32), iw: u32, ih: u32, thumbnail: bool, zoom: Option<f64>) -> (f64, f64) {
    let fit = (area.0 as f64 / iw as f64).min(area.1 as f64 / ih as f64);
    let fit = if thumbnail { fit } else { fit.min(1.0) };
    (zoom.unwrap_or(fit), fit)
}

fn place(area: Rect, iw: u32, ih: u32, scale: f64, fit: f64, pan: (f64, f64)) -> View {
    let (dw, dh) = ((iw as f64 * scale).round().max(1.0), (ih as f64 * scale).round().max(1.0));
    let reach_x = ((dw - area.w as f64) / 2.0).max(0.0);
    let reach_y = ((dh - area.h as f64) / 2.0).max(0.0);
    let pan = (pan.0.clamp(-reach_x, reach_x), pan.1.clamp(-reach_y, reach_y));
    let x = area.x as f64 + (area.w as f64 - dw) / 2.0 + pan.0;
    let y = area.y as f64 + (area.h as f64 - dh) / 2.0 + pan.1;
    View { area, dest: Rect::new(x.round() as i32, y.round() as i32, dw as i32, dh as i32), scale, fit, pan }
}

/// The popup's outline and the space inside it for content.
#[derive(Clone, Copy)]
struct Frame {
    rect: Rect,
    area: Rect,
}

/// Whether to make a smooth copy, or leave it to the nearest pixels: when it wouldn't shrink, or
/// is so nearly full size and so far past the popup that it's hardly seen.
fn worth_smoothing(view: &View) -> bool {
    view.scale < 1.0 && view.dest.w as i64 * view.dest.h as i64 <= view.area.w as i64 * view.area.h as i64 * SMOOTH_MAX_AREAS
}

impl App {
    fn paint_video_controls(&mut self, c: &mut Canvas, frame: Frame, state: PlayState, text_y: i32) {
        let co = self.colors;
        let controls = self.video_controls(frame);
        let middle = controls.play.y + controls.play.h / 2;
        let size = self.px(6.0);
        let cx = controls.play.x + controls.play.w / 2;
        if state.paused {
            // A triangle, a row at a time.
            for dy in -size..=size {
                let reach = (size - dy.abs()) * 2;
                c.fill(Rect::new(cx - size + 1, middle + dy, reach.max(1), 1), co.foreground);
            }
        } else {
            let bar = self.px(3.0).max(2);
            c.fill(Rect::new(cx - bar - 1, middle - size, bar, size * 2), co.foreground);
            c.fill(Rect::new(cx + 1, middle - size, bar, size * 2), co.foreground);
        }
        let time = format!("{} / {}", clock(state.position), clock(state.duration));
        self.text.draw_fitted(c, &time, controls.time_x as f32, text_y, (controls.seek.x - controls.time_x - self.px(4.0)).max(0) as f32, co.muted);
        let along = if state.duration > 0.0 { state.position / state.duration } else { 0.0 };
        self.paint_track(c, controls.seek, along);
        // A speaker: a box, and a cone that widens. Muted, it turns the danger color.
        let ink = if state.muted { co.danger } else { co.foreground };
        let mx = controls.mute.x + controls.mute.w / 2 - size;
        c.fill(Rect::new(mx, middle - size / 2, size / 2 + 1, size), ink);
        for i in 0..size {
            c.fill(Rect::new(mx + size / 2 + 1 + i, middle - size / 2 - i * 3 / 4, 1, size + i * 3 / 2), ink);
        }
        let volume = if state.muted { 0.0 } else { state.volume / 100.0 };
        self.paint_track(c, controls.volume, volume);
    }

    fn paint_track(&mut self, c: &mut Canvas, track: Rect, along: f64) {
        if track.w <= 0 {
            return;
        }
        let co = self.colors;
        let h = self.px(4.0).max(2);
        let y = track.y + (track.h - h) / 2;
        c.rrect(Rect::new(track.x, y, track.w, h), h / 2, co.hover);
        let done = (track.w as f64 * along.clamp(0.0, 1.0)) as i32;
        c.rrect(Rect::new(track.x, y, done, h), h / 2, co.accent);
        let knob = self.px(10.0);
        c.rrect(Rect::new(track.x + done - knob / 2, track.y + (track.h - knob) / 2, knob, knob), knob / 2, co.accent);
    }

    // ---- Opening, moving and closing ----

    fn path_at_cursor(&self) -> Option<PathBuf> {
        let id = *self.rows.get(self.cursor)?;
        Some(self.listing.as_ref()?.path(id))
    }

    fn stat_at_cursor(&self) -> Option<(u64, i64)> {
        match self.listing.as_ref()?.stat(*self.rows.get(self.cursor)?) {
            Stat::Known { size, mtime } => Some((size, mtime)),
            _ => None,
        }
    }

    fn preview_begin(&mut self, path: PathBuf, settle: bool) {
        self.preview_gen += 1;
        let now = Instant::now();
        let old = self.preview_frame();
        // Replacing the old one stops its decoder and frees its image.
        self.preview = Some(Preview {
            generation: self.preview_gen,
            path,
            content: Content::Waiting,
            size: None,
            zoom: None,
            pan: (0.0, 0.0),
            loading_shown: false,
            continuing: old.is_some(),
            seen: self.stat_at_cursor(),
            scaled: None,
            scale_gen: 0,
            scale_wanted: (0, 0),
            scale_cancel: Arc::new(AtomicBool::new(false)),
            job: None,
            start_at: settle.then_some(now + MOVE_SETTLE),
            loading_at: Some(now + LOADING_AFTER),
            drag: None,
            scrub: None,
        });
        if !settle {
            self.preview_start_job();
        }
        if self.viewer {
            // What went wrong with the last file isn't news about this one.
            self.notices.clear();
            let name = self.preview.as_ref().map(|p| display_name(&p.path)).unwrap_or_default();
            self.cmds.push(Cmd::Title(name));
            self.tell_the_list();
        }
        self.invalidate_popup(old.map_or(Rect::default(), |f| f.rect));
    }

    fn preview_start_job(&mut self) {
        let (hub, kinds, cache_home, slow) = (self.hub.clone(), self.kinds.clone(), self.cache_home.clone(), self.slow_preview);
        let area = self.preview_geometry().area;
        let Some(p) = self.preview.as_mut() else { return };
        p.start_at = None;
        p.job = Job::start(hub, p.generation, p.path.clone(), kinds, cache_home, slow, (area.w.max(1), area.h.max(1)));
        if p.job.is_none() {
            p.content = Content::Message("The previewer couldn't start".into());
            p.loading_at = None;
        }
    }

    /// Opens the preview window on the item under the cursor, or shows that item in the one
    /// that's open.
    pub fn open_preview(&mut self) {
        if self.viewer {
            return;
        }
        if !self.cursor_shown {
            // Showing the cursor selects its row, unless there's a selection to keep.
            self.step(0, Mods { ctrl: self.sel_count > 0, ..Mods::default() });
        }
        let Some(path) = self.path_at_cursor() else { return };
        if self.previewer.is_some() {
            return self.preview_follow();
        }
        self.previewer_gen += 1;
        self.cmds.push(Cmd::OpenPreview { generation: self.previewer_gen, path: path.clone(), hidden: self.show_hidden, sort: self.sort });
        self.previewer = Some(path);
    }

    /// The preview window closes: from the list, by telling it to; in it, by quitting.
    pub fn close_preview(&mut self) {
        if self.viewer {
            self.cmds.push(Cmd::Quit);
        } else if self.previewer.take().is_some() {
            self.cmds.push(Cmd::ClosePreview);
        }
    }

    /// Space in the list: opens the window, or closes the one that's open.
    pub fn toggle_preview(&mut self) {
        if self.previewer.is_some() { self.close_preview() } else { self.open_preview() }
    }

    /// Goes to the next or previous file. The selection follows, as with plain arrows, unless
    /// that would lose a selection of several things.
    fn preview_move(&mut self, delta: isize) {
        let keep_selection = self.sel_count > 1;
        self.step(delta, Mods { ctrl: keep_selection, ..Mods::default() });
        if let Some(path) = self.path_at_cursor()
            && self.preview.as_ref().is_some_and(|p| p.path != path)
        {
            self.preview_begin(path, true);
        }
    }

    /// The folder changed underneath: the file showing might be gone or overwritten, so show
    /// what's there. Only ever compares the listing with itself, as its numbers can differ from
    /// the file's own (it describes a symlink, not what it points at).
    pub fn preview_sync(&mut self) {
        if self.preview.is_none() || self.loading {
            return;
        }
        let Some(path) = self.path_at_cursor() else { return self.close_preview() };
        let stat = self.stat_at_cursor();
        let Some(p) = self.preview.as_mut() else { return };
        let overwritten = stat.is_some() && p.seen.is_some() && stat != p.seen;
        if p.seen.is_none() {
            p.seen = stat;
        }
        // A playing video keeps what it has open, and one still being written shouldn't go back to
        // the start every time it grows.
        let overwritten = overwritten && !matches!(p.content, Content::Video(_));
        if p.path != path || overwritten {
            self.preview_begin(path, false);
        }
    }

    pub fn preview_on_timer(&mut self, now: Instant) {
        let Some(p) = self.preview.as_mut() else { return };
        if p.loading_at.is_some_and(|t| t <= now) {
            p.loading_at = None;
            p.loading_shown = true;
            self.invalidate(self.preview_rect());
        }
        if self.preview.as_ref().is_some_and(|p| p.start_at.is_some_and(|t| t <= now)) {
            self.preview_start_job();
        }
    }

    pub fn preview_deadline(&self) -> Option<Instant> {
        let p = self.preview.as_ref()?;
        [p.start_at, p.loading_at].into_iter().flatten().min()
    }

    pub fn on_preview(&mut self, e: Event) {
        let old = self.preview_rect();
        let Some(p) = self.preview.as_mut().filter(|p| p.generation == e.generation) else { return };
        match e.found {
            Found::Thumbnail(image) => {
                if matches!(p.content, Content::Waiting) {
                    p.content = Content::Image { image, thumbnail: true };
                    p.loading_at = None;
                    p.continuing = true;
                }
            }
            Found::Full { image, size } => {
                p.content = Content::Image { image, thumbnail: false };
                p.size = Some(size);
                p.loading_at = None;
                p.job = None;
                self.preview_rescale();
            }
            Found::Message(text) => {
                p.content = Content::Message(text);
                p.loading_at = None;
                p.job = None;
            }
            Found::Info(info) => {
                p.content = Content::Info(info);
                p.loading_at = None;
                p.job = None;
            }
            Found::VideoStarted { remote, frames } => {
                let area = self.preview_geometry().area;
                let Some(p) = self.preview.as_mut() else { return };
                let mut remote = remote;
                // The window may have changed since the player was started.
                remote.set_size(area.w.max(1), area.h.max(1));
                // What it starts as, so a key pressed before it first reports does the right thing.
                let state = PlayState { volume: 100.0, ..PlayState::default() };
                p.content = Content::Video(Video { remote, frames, frame: None, state, frames_shown: 0, settled_at: Instant::now() });
            }
            Found::VideoFrame { index, w, h } => {
                if let Content::Video(video) = &mut p.content {
                    let first = video.frame.is_none();
                    video.frame = Some((index, w, h));
                    video.frames_shown += 1;
                    p.loading_at = None;
                    // Only the picture changes, so only the picture gets painted.
                    if !first {
                        let area = self.preview_geometry().area;
                        return self.invalidate(area);
                    }
                }
            }
            Found::VideoState(state) => {
                if let Content::Video(video) = &mut p.content {
                    if Instant::now() < video.settled_at {
                        video.state.position = state.position;
                        video.state.duration = state.duration;
                    } else {
                        video.state = state;
                    }
                    let r = self.preview_geometry();
                    let foot = Rect::new(r.rect.x, r.area.bottom(), r.rect.w, r.rect.bottom() - r.area.bottom());
                    return self.invalidate(foot);
                }
            }
            Found::Scaled { scale_gen, w, h, rgba } => {
                if scale_gen == p.scale_gen {
                    p.scaled = Some(Scaled { w, h, rgba });
                }
            }
        }
        self.invalidate_popup(old);
    }

    // ---- Where things go ----

    /// The whole window: a line for the name, the picture, and a line for what it is.
    fn preview_geometry(&self) -> Frame {
        let (title, foot) = (self.px(TITLE_H), self.px(FOOT_H));
        Frame { rect: Rect::new(0, 0, self.w, self.h), area: Rect::new(0, title, self.w, (self.h - title - foot).max(0)) }
    }

    fn preview_frame(&self) -> Option<Frame> {
        self.preview.as_ref()?;
        Some(self.preview_geometry())
    }

    fn video_controls(&self, frame: Frame) -> Controls {
        let (y, h) = (frame.area.bottom(), self.px(FOOT_H));
        let pad = self.px(10.0);
        let button = self.px(26.0);
        let play = Rect::new(frame.rect.x + pad, y, button, h);
        let time_x = play.right() + self.px(6.0);
        let volume_w = self.px(70.0);
        let volume = Rect::new(frame.rect.right() - pad - volume_w, y, volume_w, h);
        let mute = Rect::new(volume.x - button - self.px(2.0), y, button, h);
        let seek_x = time_x + self.px(104.0);
        let seek = Rect::new(seek_x, y, (mute.x - self.px(10.0) - seek_x).max(0), h);
        Controls { play, time_x, seek, mute, volume }
    }

    pub fn preview_rect(&self) -> Rect {
        self.preview_frame().map_or(Rect::default(), |f| f.rect)
    }

    /// Repaints the popup, and what it covered if it's only just appeared or gone.
    fn invalidate_popup(&mut self, old: Rect) {
        self.invalidate(old.union(self.preview_rect()));
    }

    fn preview_view(&self) -> Option<View> {
        let p = self.preview.as_ref()?;
        let Content::Image { image, thumbnail } = &p.content else { return None };
        let frame = self.preview_frame()?;
        if frame.area.is_empty() {
            return None;
        }
        let (scale, fit) = image_scale((frame.area.w, frame.area.h), image.w, image.h, *thumbnail, p.zoom);
        Some(place(frame.area, image.w, image.h, scale, fit, p.pan))
    }

    /// Shrinking a big image well takes longer than a frame, so it happens off to the side.
    /// Until it's back, the nearest pixels stand in.
    pub fn preview_rescale(&mut self) {
        let area = self.preview_geometry().area;
        if let Some(Preview { content: Content::Video(video), .. }) = self.preview.as_mut() {
            video.remote.set_size(area.w.max(1), area.h.max(1));
        }
        let Some(view) = self.preview_view() else { return };
        let hub = self.hub.clone();
        let Some(p) = self.preview.as_mut() else { return };
        let Content::Image { image, thumbnail: false } = &p.content else { return };
        let wanted = (view.dest.w, view.dest.h);
        let smooth = worth_smoothing(&view);
        if p.scaled.as_ref().is_some_and(|s| (s.w, s.h) == wanted) || (p.scale_wanted == wanted && smooth) {
            return;
        }
        p.scale_cancel.store(true, Ordering::Relaxed);
        p.scaled = None;
        p.scale_gen += 1;
        p.scale_wanted = (0, 0);
        if !smooth {
            return;
        }
        p.scale_wanted = wanted;
        let cancel = Arc::new(AtomicBool::new(false));
        p.scale_cancel = cancel.clone();
        let (image, generation, scale_gen) = (image.clone(), p.generation, p.scale_gen);
        let _ = std::thread::Builder::new().name("gannet-scale".into()).spawn(move || {
            let (w, h) = wanted;
            if let Some(rgba) = box_scale(image.pixels(), image.w as usize, image.h as usize, w as usize, h as usize, &cancel) {
                hub.send(Msg::Preview(Event { generation, found: Found::Scaled { scale_gen, w, h, rgba } }));
            }
        });
    }

    // ---- Input ----

    pub fn preview_key(&mut self, sym: u32, m: Mods) {
        if m.alt {
            return;
        }
        if self.preview_video_key(sym, m) {
            return;
        }
        let pan = self.px(80.0) as f64;
        match sym {
            k::KEY_q | k::KEY_w if m.ctrl => self.request_close(),
            k::KEY_space if m.ctrl || self.key_is_repeat => {}
            k::KEY_space | k::KEY_Escape => self.close_preview(),
            k::KEY_Return | k::KEY_KP_Enter if !self.key_is_repeat => self.preview_open_file(),
            k::KEY_Left if m.shift => self.preview_pan(pan, 0.0),
            k::KEY_Right if m.shift => self.preview_pan(-pan, 0.0),
            k::KEY_Up if m.shift => self.preview_pan(0.0, pan),
            k::KEY_Down if m.shift => self.preview_pan(0.0, -pan),
            k::KEY_Left | k::KEY_Up => self.preview_move(-1),
            k::KEY_Right | k::KEY_Down => self.preview_move(1),
            k::KEY_plus | k::KEY_equal | k::KEY_KP_Add => self.preview_zoom(ZOOM_STEP),
            k::KEY_minus | k::KEY_underscore | k::KEY_KP_Subtract => self.preview_zoom(1.0 / ZOOM_STEP),
            k::KEY_0 | k::KEY_KP_0 => self.preview_zoom_fit(),
            _ => {}
        }
    }

    fn video_mut(&mut self) -> Option<&mut Video> {
        match self.preview.as_mut() {
            Some(Preview { content: Content::Video(video), .. }) => Some(video),
            _ => None,
        }
    }

    /// Arrows without Shift still go to other files, and Space still closes.
    fn preview_video_key(&mut self, sym: u32, m: Mods) -> bool {
        if m.ctrl {
            return false;
        }
        let repeat = self.key_is_repeat;
        if self.video_mut().is_none() {
            return false;
        }
        match sym {
            k::KEY_Return | k::KEY_KP_Enter | k::KEY_p | k::KEY_P => {
                if !repeat {
                    self.toggle_pause();
                }
            }
            k::KEY_m | k::KEY_M => {
                if !repeat {
                    self.toggle_mute();
                }
            }
            k::KEY_Left if m.shift => self.video_seek(-SEEK_STEP),
            k::KEY_Right if m.shift => self.video_seek(SEEK_STEP),
            k::KEY_Up if m.shift => self.video_volume(VOLUME_STEP),
            k::KEY_Down if m.shift => self.video_volume(-VOLUME_STEP),
            _ => return false,
        }
        true
    }

    /// These change what's shown straight away, so a second press before the player has answered
    /// builds on the first rather than repeating it.
    fn toggle_pause(&mut self) {
        if let Some(video) = self.video_mut() {
            video.state.paused = !video.state.paused;
            video.settled_at = Instant::now() + REPORTS_CATCH_UP;
            let paused = video.state.paused;
            video.remote.set_paused(paused);
        }
    }

    fn toggle_mute(&mut self) {
        if let Some(video) = self.video_mut() {
            video.state.muted = !video.state.muted;
            video.settled_at = Instant::now() + REPORTS_CATCH_UP;
            let muted = video.state.muted;
            video.remote.set_muted(muted);
        }
    }

    fn video_volume(&mut self, by: f64) {
        if let Some(video) = self.video_mut() {
            video.state.volume = (video.state.volume + by).clamp(0.0, 100.0);
            video.settled_at = Instant::now() + REPORTS_CATCH_UP;
            let volume = video.state.volume;
            video.remote.set_volume(volume);
        }
    }

    fn video_seek(&mut self, by: f64) {
        if let Some(video) = self.video_mut() {
            video.remote.seek_by(by);
        }
    }

    fn real_image(&self) -> bool {
        self.preview.as_ref().is_some_and(|p| matches!(p.content, Content::Image { thumbnail: false, .. }))
    }

    fn preview_zoom(&mut self, by: f64) {
        if !self.real_image() {
            return;
        }
        let Some(view) = self.preview_view() else { return };
        let old = self.preview_rect();
        let next = (view.scale * by).min(ZOOM_MAX);
        let Some(p) = self.preview.as_mut() else { return };
        if next <= view.fit {
            p.zoom = None;
            p.pan = (0.0, 0.0);
        } else {
            // Keeps what's in the middle of the view in the middle.
            let grew = next / view.scale;
            p.zoom = Some(next);
            p.pan = (view.pan.0 * grew, view.pan.1 * grew);
        }
        self.preview_rescale();
        self.invalidate_popup(old);
    }

    fn preview_zoom_fit(&mut self) {
        let old = self.preview_rect();
        if let Some(p) = self.preview.as_mut() {
            p.zoom = None;
            p.pan = (0.0, 0.0);
        }
        self.preview_rescale();
        self.invalidate_popup(old);
    }

    fn preview_pan(&mut self, dx: f64, dy: f64) {
        let Some(view) = self.preview_view() else { return };
        if let Some(p) = self.preview.as_mut() {
            p.pan = (view.pan.0 + dx, view.pan.1 + dy);
        }
        self.preview_settle_pan();
        self.invalidate(self.preview_rect());
    }

    /// Stops a pan past the edge from building up where it can't be seen.
    fn preview_settle_pan(&mut self) {
        let Some(view) = self.preview_view() else { return };
        if let Some(p) = self.preview.as_mut() {
            p.pan = view.pan;
        }
    }

    pub fn preview_button(&mut self, button: u32, hit: Hit) {
        match button {
            BTN_SIDE => return self.preview_move(-1),
            BTN_EXTRA => return self.preview_move(1),
            BTN_LEFT => {}
            _ => return,
        }
        if hit == Hit::Outside {
            return self.close_preview();
        }
        let (x, y) = self.pointer;
        if self.video_mut().is_some() {
            return self.video_click(x, y);
        }
        if self.info_open_rect().is_some_and(|r| r.contains(x, y)) {
            return self.preview_open_file();
        }
        let Some(view) = self.preview_view() else { return };
        if view.dest.w > view.area.w || view.dest.h > view.area.h {
            let at = view.pan;
            if let Some(p) = self.preview.as_mut() {
                p.drag = Some((x, y, at.0, at.1));
            }
        }
    }

    pub fn preview_release(&mut self) {
        if let Some(p) = self.preview.as_mut() {
            p.drag = None;
            p.scrub = None;
        }
    }

    fn video_click(&mut self, x: i32, y: i32) {
        let frame = self.preview_geometry();
        let c = self.video_controls(frame);
        if c.play.contains(x, y) || frame.area.contains(x, y) {
            self.toggle_pause();
        } else if c.mute.contains(x, y) {
            self.toggle_mute();
        } else if c.seek.contains(x, y) {
            self.video_scrub(Scrub::Seek, x);
        } else if c.volume.contains(x, y) {
            self.video_scrub(Scrub::Volume, x);
        }
    }

    fn video_scrub(&mut self, what: Scrub, x: i32) {
        let c = self.video_controls(self.preview_geometry());
        if let Some(p) = self.preview.as_mut() {
            p.scrub = Some(what);
        }
        let Some(video) = self.video_mut() else { return };
        match what {
            Scrub::Seek => video.remote.seek_to(fraction_along(c.seek, x)),
            Scrub::Volume => video.remote.set_volume(fraction_along(c.volume, x) * 100.0),
        }
    }

    /// Returns whether the pointer is busy dragging the image.
    pub fn preview_drag(&mut self, x: i32, y: i32) -> bool {
        let Some(p) = self.preview.as_mut() else { return false };
        if let Some(what) = p.scrub {
            self.video_scrub(what, x);
            return true;
        }
        let Some((sx, sy, px, py)) = p.drag else { return false };
        p.pan = (px + (x - sx) as f64, py + (y - sy) as f64);
        self.preview_settle_pan();
        self.invalidate(self.preview_rect());
        true
    }

    // ---- Drawing ----

    pub fn paint_preview(&mut self, c: &mut Canvas) {
        let Some(Frame { rect: r, area }) = self.preview_frame() else { return };
        if r.is_empty() || !c.clip().intersects(r) {
            return;
        }
        let view = self.preview_view();
        let open = self.info_open_rect();
        let Some(p) = self.preview.take() else { return };
        let co = self.colors;
        c.fill(r, co.surface);
        c.fill(Rect::new(r.x, area.y - 1, r.w, 1), co.border);
        c.fill(Rect::new(r.x, area.bottom(), r.w, 1), co.border);
        let pad = self.px(14.0);
        let max = (r.w - pad * 2).max(0) as f32;
        let name = display_name(&p.path);
        let title_y = self.baseline(r.y, self.px(TITLE_H));
        self.text.draw_fitted(c, &name, (r.x + pad) as f32, title_y, max, co.foreground);
        if !area.is_empty() {
            c.fill(area, co.base);
            match (&p.content, &view) {
                (Content::Image { image, .. }, Some(view)) => {
                    App::with_clip(c, area, |c| match &p.scaled {
                        Some(s) if (s.w, s.h) == (view.dest.w, view.dest.h) => c.image(&s.rgba, s.w, s.h, view.dest),
                        _ => c.image(image.pixels(), image.w as i32, image.h as i32, view.dest),
                    });
                }
                (Content::Video(Video { frame: Some((index, w, h)), frames, .. }), _) => {
                    let (scale, fit) = image_scale((area.w, area.h), *w, *h, true, None);
                    let dest = place(area, *w, *h, scale, fit, (0.0, 0.0)).dest;
                    App::with_clip(c, area, |c| c.xrgb(frames.frame(*index, *w, *h), *w as i32, *h as i32, dest));
                }
                (Content::Message(text), _) => self.paint_center(c, area, text, co.muted),
                (Content::Info(info), _) => self.paint_info(c, area, info, open),
                _ if p.loading_shown => self.paint_center(c, area, "Loading…", co.muted),
                _ => {}
            }
            let foot = match (&p.content, &view) {
                (Content::Image { image, thumbnail: false }, Some(view)) => {
                    let size = p.size.map_or_else(String::new, |s| format!("  ·  {}", format::size(s)));
                    format!("{} × {}{size}  ·  {:.0}%", image.w, image.h, view.scale * 100.0)
                }
                (Content::Image { thumbnail: true, .. }, _) => "Loading the full image…".to_string(),
                (Content::Info(info), _) => info.mime.clone(),
                _ => String::new(),
            };
            let foot_y = self.baseline(area.bottom(), self.px(FOOT_H));
            // The window has no other place for something going wrong, like a file that
            // wouldn't open.
            if let Some(notice) = self.notices.last().cloned() {
                self.text.draw_fitted(c, &notice, (r.x + pad) as f32, foot_y, max, co.danger);
            } else if let Content::Video(video) = &p.content {
                self.paint_video_controls(c, Frame { rect: r, area }, video.state, foot_y);
            } else {
                self.text.draw_fitted(c, &foot, (r.x + pad) as f32, foot_y, max, co.muted);
            }
        }
        self.preview = Some(p);
    }
}

/// A file's name as it's shown, or its whole path if it has none.
fn display_name(path: &Path) -> String {
    path.file_name().map_or_else(|| path.to_string_lossy().into_owned(), |n| n.to_string_lossy().into_owned())
}

/// Where the parts of the view for any file go.
struct InfoLayout {
    picture: Rect,
    lines: [Rect; 2],
    open: Rect,
}

impl App {
    fn info_layout(&self, area: Rect) -> InfoLayout {
        let side = ((area.w.min(area.h) as f64 * 0.4) as i32).clamp(self.px(48.0), self.px(256.0));
        let (gap, line, button) = (self.px(18.0), self.px(24.0), self.px(32.0));
        let top = area.y + (area.h - (side + gap + line * 2 + gap + button)).max(0) / 2;
        let picture = Rect::new(area.x + (area.w - side) / 2, top, side, side);
        let first = Rect::new(area.x, picture.bottom() + gap, area.w, line);
        let second = Rect::new(area.x, first.bottom(), area.w, line);
        let open_w = self.px(104.0);
        let open = Rect::new(area.x + (area.w - open_w) / 2, second.bottom() + gap, open_w, button);
        InfoLayout { picture, lines: [first, second], open }
    }

    /// The Open button, when the view for any file is showing.
    fn info_open_rect(&self) -> Option<Rect> {
        let p = self.preview.as_ref()?;
        matches!(p.content, Content::Info(_)).then(|| self.info_layout(self.preview_geometry().area).open)
    }

    fn paint_info(&mut self, c: &mut Canvas, area: Rect, info: &Info, open: Option<Rect>) {
        let co = self.colors;
        let l = self.info_layout(area);
        match &info.thumbnail {
            Some(image) => {
                let (scale, fit) = image_scale((l.picture.w, l.picture.h), image.w, image.h, false, None);
                let dest = place(l.picture, image.w, image.h, scale, fit, (0.0, 0.0)).dest;
                App::with_clip(c, area, |c| c.image(image.pixels(), image.w as i32, image.h as i32, dest));
            }
            None => self.icons.draw(c, info.icon, l.picture.x, l.picture.y, l.picture.w, co.muted),
        }
        let now = gannet_core::time::now();
        let details: Vec<String> = [info.size.map(format::size), info.modified.map(|m| format!("Modified {}", format::date(m, now)))].into_iter().flatten().collect();
        self.paint_line(c, l.lines[0], &info.description, co.foreground);
        self.paint_line(c, l.lines[1], &details.join("  ·  "), co.muted);
        if let Some(open) = open {
            c.rrect(open, self.px(6.0), co.accent);
            self.paint_line(c, open, "Open", co.on_accent);
        }
    }

    /// One line of text in the middle of `r`, cut short if it doesn't fit.
    fn paint_line(&mut self, c: &mut Canvas, r: Rect, text: &str, color: u32) {
        let pad = self.px(12.0);
        let max = (r.w - pad * 2).max(0) as f32;
        let w = self.text.measure(text).min(max);
        let x = r.x as f32 + (r.w as f32 - w) / 2.0;
        let y = self.baseline(r.y, r.h);
        self.text.draw_fitted(c, text, x, y, max, color);
    }

    /// Enter in the preview window: the file opens in its app, and the preview stays.
    fn preview_open_file(&mut self) {
        let Some(path) = self.preview.as_ref().map(|p| p.path.clone()) else { return };
        let hub = self.hub.clone();
        gannet_core::open::open_with_default(&path, move |p, why| hub.send(Msg::Notice(format!("Couldn't open {}: {why}", display_name(&p)))));
    }

    // ---- The preview window's side ----

    /// Sets this app up as the preview window, showing `path` straight away while its folder is
    /// read for the arrows to walk through.
    pub fn start_viewer(&mut self, path: PathBuf) {
        self.viewer = true;
        let dir = path.parent().map_or_else(|| PathBuf::from("/"), Path::to_path_buf);
        let name = path.file_name().map(|n| n.as_encoded_bytes().to_vec());
        // A hidden file has to be in the listing, or the cursor can't be on it.
        if name.as_ref().is_some_and(|n| n.first() == Some(&b'.')) {
            self.show_hidden = true;
        }
        self.previewer = Some(path.clone());
        self.go(Stop { dir, cursor_name: name });
        self.preview_begin(path, false);
    }

    /// The list moved its cursor: show what it's on now.
    pub fn on_preview_show(&mut self, path: PathBuf) {
        if !self.viewer || self.preview.as_ref().is_some_and(|p| p.path == path) {
            return;
        }
        self.previewer = Some(path.clone());
        let name = path.file_name().map(|n| n.as_encoded_bytes().to_vec());
        if name.as_ref().is_some_and(|n| n.first() == Some(&b'.')) && !self.show_hidden {
            self.show_hidden = true;
            self.rederive(false);
        }
        let dir = path.parent().map_or_else(|| PathBuf::from("/"), Path::to_path_buf);
        let row = self.listing.as_ref().filter(|l| l.dir == dir && !self.loading).and_then(|l| name.as_ref().and_then(|n| self.rows.iter().position(|&i| *l.key(i) == **n)));
        match row {
            Some(row) => self.move_cursor(row, Mods::default()),
            None => self.go(Stop { dir, cursor_name: name }),
        }
        // Settling, as the list's arrows can run through files as fast as the key repeats.
        self.preview_begin(path, true);
    }

    /// Tells the list which file is showing, if it doesn't know.
    fn tell_the_list(&mut self) {
        let Some(path) = self.preview.as_ref().map(|p| p.path.clone()) else { return };
        if self.previewer.as_ref() != Some(&path) {
            self.previewer = Some(path.clone());
            self.cmds.push(Cmd::PreviewStepped(path));
        }
    }

    // ---- The list's side ----

    /// Tells the preview window where the cursor is now, if it's moved.
    pub fn preview_follow(&mut self) {
        if self.viewer || self.previewer.is_none() || !self.cursor_shown {
            return;
        }
        let Some(path) = self.path_at_cursor() else { return };
        if self.previewer.as_ref() != Some(&path) {
            self.previewer = Some(path.clone());
            self.cmds.push(Cmd::PreviewShow(path));
        }
    }

    /// The preview window stepped to another file: the cursor follows, and the selection with
    /// it as arrows do, unless that would lose a selection of several things.
    pub fn on_preview_stepped(&mut self, generation: u64, path: PathBuf) {
        if generation != self.previewer_gen || self.previewer.is_none() {
            return;
        }
        self.previewer = Some(path.clone());
        if path.parent() != Some(self.dir.as_path()) {
            return;
        }
        let Some(name) = path.file_name().map(|n| n.as_encoded_bytes()) else { return };
        let Some(row) = self.listing.as_ref().and_then(|l| self.rows.iter().position(|&i| *l.key(i) == *name)) else { return };
        if row != self.cursor || !self.cursor_shown {
            self.move_cursor(row, Mods { ctrl: self.sel_count > 1, ..Mods::default() });
        }
    }

    /// The preview window has gone. Said so if it wasn't asked to.
    pub fn on_preview_ended(&mut self, generation: u64, crashed: bool) {
        if generation != self.previewer_gen {
            return;
        }
        self.previewer = None;
        if crashed {
            self.notice("The preview stopped unexpectedly".to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn not_cancelled() -> AtomicBool {
        AtomicBool::new(false)
    }

    #[test]
    fn scaling_averages() {
        let src = [0, 0, 0, 255, 100, 100, 100, 255, 200, 200, 200, 255, 100, 100, 100, 255];
        let out = box_scale(&src, 2, 2, 1, 1, &not_cancelled()).unwrap();
        assert_eq!(out, [100, 100, 100, 255]);
    }

    #[test]
    fn transparent_pixels_dont_bleed_their_color() {
        // Fully transparent black beside solid red: the color stays red, half as solid.
        let src = [0, 0, 0, 0, 255, 0, 0, 255];
        let out = box_scale(&src, 2, 1, 1, 1, &not_cancelled()).unwrap();
        assert_eq!(out, [255, 0, 0, 127]);
    }

    #[test]
    fn scaling_handles_any_ratio_and_stops_when_cancelled() {
        let src = vec![200u8; 7 * 5 * 4];
        for (dw, dh) in [(1, 1), (3, 2), (6, 4), (7, 5)] {
            let out = box_scale(&src, 7, 5, dw, dh, &not_cancelled()).unwrap();
            assert_eq!(out.len(), dw * dh * 4);
            assert!(out.iter().all(|&b| b == 200), "{dw}x{dh}");
        }
        assert!(box_scale(&src, 7, 5, 3, 2, &AtomicBool::new(true)).is_none());
    }

    const AREA: Rect = Rect::new(10, 20, 1000, 500);

    fn view(iw: u32, ih: u32, thumbnail: bool, zoom: Option<f64>, pan: (f64, f64)) -> View {
        let (scale, fit) = image_scale((AREA.w, AREA.h), iw, ih, thumbnail, zoom);
        place(AREA, iw, ih, scale, fit, pan)
    }

    #[test]
    fn big_images_fit_and_small_ones_stay_their_own_size() {
        let v = view(4000, 1000, false, None, (0.0, 0.0));
        assert_eq!(v.scale, 0.25);
        assert_eq!(v.dest, Rect::new(10, 145, 1000, 250));
        let v = view(100, 50, false, None, (0.0, 0.0));
        assert_eq!(v.scale, 1.0);
        assert_eq!(v.dest, Rect::new(460, 245, 100, 50));
    }

    #[test]
    fn a_thumbnail_fills_the_space_the_real_image_will() {
        assert_eq!(view(100, 50, true, None, (0.0, 0.0)).dest, AREA);
    }

    #[test]
    fn panning_stops_at_the_edges_and_needs_a_zoomed_image() {
        let v = view(2000, 1000, false, Some(1.0), (5000.0, -5000.0));
        assert_eq!(v.pan, (500.0, -250.0));
        assert_eq!(v.dest.x, AREA.x);
        assert_eq!(v.dest.bottom(), AREA.bottom());
        assert_eq!(view(100, 50, false, None, (30.0, 30.0)).pan, (0.0, 0.0));
    }

    #[test]
    fn clocks_read_right() {
        assert_eq!(clock(0.0), "0:00");
        assert_eq!(clock(65.9), "1:05");
        assert_eq!(clock(3661.0), "1:01:01");
        assert_eq!(clock(-5.0), "0:00");
    }

    #[test]
    fn clicks_on_a_track_stay_on_it() {
        let track = Rect::new(100, 0, 200, 10);
        assert_eq!(fraction_along(track, 100), 0.0);
        assert_eq!(fraction_along(track, 200), 0.5);
        assert_eq!(fraction_along(track, 5000), 1.0);
        assert_eq!(fraction_along(track, -5), 0.0);
        assert_eq!(fraction_along(Rect::new(0, 0, 0, 0), 5), 0.0, "a track squeezed to nothing");
    }

    #[test]
    fn extreme_shapes_still_get_a_pixel() {
        let v = view(65_535, 1, false, None, (0.0, 0.0));
        assert!(v.dest.w >= 1 && v.dest.h >= 1);
    }

    #[test]
    fn only_a_smooth_copy_worth_its_memory_gets_made() {
        // Fitted, and a little past the popup.
        assert!(worth_smoothing(&view(12_000, 9_000, false, None, (0.0, 0.0))));
        assert!(worth_smoothing(&view(12_000, 9_000, false, Some(0.1), (0.0, 0.0))));
        // Nearly full size: 350 MB for a window that shows a small part of it.
        assert!(!worth_smoothing(&view(12_000, 9_000, false, Some(0.9), (0.0, 0.0))));
        // Not shrinking at all.
        assert!(!worth_smoothing(&view(100, 50, false, Some(2.0), (0.0, 0.0))));
    }

    fn sleeper(script: &str) -> Helper {
        let output = memfd_create("test", MemfdFlags::CLOEXEC).unwrap();
        let mut command = Command::new("sh");
        command.args(["-c", script]).stdin(Stdio::null());
        Helper::run(command, output).unwrap()
    }

    fn event() -> OwnedFd {
        eventfd(0, EventfdFlags::CLOEXEC | EventfdFlags::NONBLOCK).unwrap()
    }

    #[test]
    fn closing_stderr_and_carrying_on_doesnt_hide_a_running_process() {
        let cancel = event();
        let mut helper = sleeper("exec 2>&-; exec sleep 30");
        let start = Instant::now();
        assert!(matches!(helper.wait(&cancel, Duration::from_millis(100)), Waited::TimedOut));
        assert!(start.elapsed() < Duration::from_secs(5), "the limit still holds");
        rustix::io::write(&cancel, &1u64.to_ne_bytes()).unwrap();
        assert!(matches!(helper.wait(&cancel, Duration::from_secs(5)), Waited::Cancelled), "and so does cancelling");
    }

    #[test]
    fn an_exit_is_seen_with_its_code_and_asking_again_gives_the_same_answer() {
        let cancel = event();
        let mut helper = sleeper("exit 3");
        for _ in 0..2 {
            let Waited::Exited(status) = helper.wait(&cancel, Duration::from_secs(5)) else { panic!("should have exited") };
            assert_eq!(status.code(), Some(3));
        }
    }

    fn memfd_with(bytes: &[u8]) -> OwnedFd {
        let fd = memfd_create("test", MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING).unwrap();
        rustix::io::write(&fd, bytes).unwrap();
        fd
    }

    fn wrote(w: u32, h: u32, pixels: &[u8]) -> Vec<u8> {
        let mut out = decode::MAGIC.to_vec();
        out.extend_from_slice(&w.to_ne_bytes());
        out.extend_from_slice(&h.to_ne_bytes());
        out.extend_from_slice(pixels);
        out
    }

    #[test]
    fn reads_what_a_decoder_wrote() {
        let image = ImageBuf::from_memfd(&memfd_with(&wrote(1, 2, &[1, 2, 3, 4, 5, 6, 7, 8]))).unwrap();
        assert_eq!((image.w, image.h), (1, 2));
        assert_eq!(image.pixels(), [1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn doesnt_believe_a_bad_header() {
        // Wrong size for the claim, overflowing claims, no pixels, wrong magic, and too short.
        assert!(ImageBuf::from_memfd(&memfd_with(&wrote(2, 2, &[0; 8]))).is_none());
        assert!(ImageBuf::from_memfd(&memfd_with(&wrote(u32::MAX, u32::MAX, &[0; 8]))).is_none());
        assert!(ImageBuf::from_memfd(&memfd_with(&wrote(0, 0, &[]))).is_none());
        assert!(ImageBuf::from_memfd(&memfd_with(b"NOPE\0\0\0\0\0\0\0\0")).is_none());
        assert!(ImageBuf::from_memfd(&memfd_with(b"GN")).is_none());
    }

    #[test]
    fn exit_codes_become_the_right_words() {
        let fd = memfd_with(b"");
        let said = |raw: i32| match answer(ExitStatus::from_raw(raw), &fd, 0) {
            Some(Found::Message(m)) => m,
            _ => panic!("expected a message"),
        };
        let exit = |code: i32| code << 8;
        assert!(answer(ExitStatus::from_raw(exit(decode::EXIT_UNSUPPORTED)), &fd, 0).is_none(), "not known, so something else says what it is");
        assert_eq!(said(exit(decode::EXIT_TOO_LARGE)), TOO_LARGE);
        assert_eq!(said(exit(decode::EXIT_DAMAGED)), DAMAGED);
        assert_eq!(said(exit(decode::EXIT_NO_SANDBOX)), NO_SANDBOX);
        assert_eq!(said(exit(101)), DAMAGED); // A decoder panic.
        assert_eq!(said(libc::SIGABRT), TOO_LARGE); // Out of memory.
        assert_eq!(said(libc::SIGKILL), TOO_LARGE); // Out of CPU time.
        assert_eq!(said(libc::SIGSEGV), DAMAGED);
        assert_eq!(said(libc::SIGSYS), SANDBOX_STOPPED);
        assert_eq!(said(exit(0)), DAMAGED); // Said it worked but wrote nothing.
    }
}
