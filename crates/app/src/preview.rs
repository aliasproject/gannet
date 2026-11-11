//! The preview: what's showing, the background work that fetches it, and drawing it. It runs in a
//! window of its own, in a process of its own (`gannet --preview`), which is an `App` with
//! `viewer` set: its folder listing is what the arrows walk through. The file manager's side of
//! it is at the end: opening and closing that process, and following each other's cursor.
//! Nothing here decodes. That happens in yet another process (see `decode`), and this thread
//! only ever draws pixels that are ready.

use std::collections::HashMap;
use std::ffi::OsString;
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

use gannet_core::foldersize::{self, Tally};
use gannet_core::history::Stop;
use gannet_core::listing::Stat;
use gannet_core::mime::{self, MimeDb};
use gannet_core::thumbcache;
use gannet_core::thumbnailers::Thumbnailers;
use rustix::event::{EventfdFlags, PollFd, PollFlags, Timespec, eventfd, poll};
use rustix::fs::{MemfdFlags, SealFlags, fcntl_add_seals, fstat, memfd_create};
use rustix::mm::{MapFlags, ProtFlags, mmap, munmap};
use rustix::pipe::{PipeFlags, pipe_with};
use xkbcommon::xkb::keysyms as k;

use crate::app::{App, BTN_EXTRA, BTN_LEFT, BTN_SIDE, Cmd, Hit, Hub, Msg};
use crate::canvas::{Canvas, Rect};
use crate::decode;
use crate::doc::{self, Doc};
use crate::docview;
use crate::tools;
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
const DAMAGED_TEXT: &str = "This file can't be read as text";
const TEXT_TOO_LARGE: &str = "This file took too much work to show as text";
const TOO_LARGE: &str = "This image is too large to preview";
const NO_BUBBLEWRAP: &str = "Playing video and sound needs bubblewrap, which isn't installed";
const NO_MPV: &str = "Playing video and sound needs mpv, which isn't installed";
const PLAYER_FAILED: &str = "The player couldn't start";
const NO_JAIL: &str = "The player couldn't start its sandbox";
const CANT_PLAY: &str = "This can't be played. It may be damaged.";
const SANDBOX_STOPPED: &str = "The preview was stopped by its sandbox";
const NO_SANDBOX: &str = "Previews are off: this system can't run the previewer safely";

/// Pixels in memory another process wrote, so dropping this hands them straight back.
pub struct ImageBuf {
    ptr: *mut std::ffi::c_void,
    len: usize,
    pub w: u32,
    pub h: u32,
    /// How long each frame shows, for an image that moves. Empty for a still one.
    pub delays: Vec<Duration>,
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
        // Unmapped again by `drop` if it turns out to be wrong.
        let mut image = Self { ptr, len, w: 0, h: 0, delays: Vec::new() };
        // SAFETY: `len` bytes are mapped until `image` is dropped, and at least `HEADER` exist.
        let bytes = unsafe { std::slice::from_raw_parts(ptr as *const u8, len) };
        let word = |at: usize| Some(u32::from_ne_bytes(bytes.get(at..at + 4)?.try_into().ok()?));
        image.w = word(4)?;
        image.h = word(8)?;
        let frame = (image.w as u64).checked_mul(image.h as u64)?.checked_mul(4)?;
        let frames = match &bytes[..4] {
            magic if magic == decode::MAGIC => 1,
            magic if magic == decode::ANIMATION_MAGIC => {
                let count = word(len.checked_sub(4)?)? as u64;
                if !(2..=decode::MAX_FRAMES as u64).contains(&count) {
                    return None;
                }
                count
            }
            _ => return None,
        };
        let pixels_end = frame.checked_mul(frames)?.checked_add(decode::HEADER as u64)?;
        let want = if frames == 1 { pixels_end } else { pixels_end.checked_add(4 * frames + 4)? };
        if image.w == 0 || image.h == 0 || want != len as u64 {
            return None;
        }
        if frames > 1 {
            image.delays = (0..frames as usize).map(|i| word(pixels_end as usize + i * 4).map(frame_delay)).collect::<Option<_>>()?;
        }
        Some(image)
    }

    /// How many pictures it holds: one, or an animation's frames.
    pub fn frames(&self) -> usize {
        self.delays.len().max(1)
    }

    /// The first frame, which is the whole of a still image.
    pub fn pixels(&self) -> &[u8] {
        self.frame(0)
    }

    pub fn frame(&self, index: usize) -> &[u8] {
        let size = self.w as usize * self.h as usize * 4;
        let start = decode::HEADER + index.min(self.frames() - 1) * size;
        // SAFETY: mapped for `len` bytes until drop, and `from_memfd` checked every frame fits.
        unsafe { std::slice::from_raw_parts((self.ptr as *const u8).add(start), size) }
    }
}

#[cfg(test)]
impl ImageBuf {
    /// A 1×1 animation with these delays, as a decoder would write it.
    pub fn animation_for_test(delays_ms: &[u32]) -> Self {
        let mut out = decode::ANIMATION_MAGIC.to_vec();
        for word in [1, 1].into_iter().chain(std::iter::repeat_n(0, delays_ms.len())).chain(delays_ms.iter().copied()).chain([delays_ms.len() as u32]) {
            out.extend_from_slice(&word.to_ne_bytes());
        }
        let fd = memfd_create("test", MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING).unwrap();
        rustix::io::write(&fd, &out).unwrap();
        Self::from_memfd(&fd).unwrap()
    }
}

/// How long a frame shows. Browsers show a frame asking for next to no time for a tenth of a
/// second, and files are made to look right in them.
fn frame_delay(ms: u32) -> Duration {
    Duration::from_millis(if ms < 20 { 100 } else { ms.min(60_000) as u64 })
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
    thumbnailer_dirs: Vec<PathBuf>,
    thumbnailers: OnceLock<Thumbnailers>,
}

impl Kinds {
    /// `dirs` are the `mime` folders, first wins; `lang` is the user's language, like `de_DE.UTF-8`.
    pub fn new(dirs: Vec<PathBuf>, lang: Option<String>) -> Self {
        Self { dirs, lang, db: OnceLock::new(), thumbnailer_dirs: Vec::new(), thumbnailers: OnceLock::new() }
    }

    /// Where the system's thumbnailers are described (`thumbnailers` folders), first wins.
    pub fn with_thumbnailers(mut self, dirs: Vec<PathBuf>) -> Self {
        self.thumbnailer_dirs = dirs;
        self
    }

    fn thumbnailers(&self) -> &Thumbnailers {
        self.thumbnailers.get_or_init(|| Thumbnailers::load(&self.thumbnailer_dirs))
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
    /// For a folder, what's in it, once counting has started.
    pub folder: Option<Tally>,
}

pub enum Found {
    Thumbnail(Arc<ImageBuf>),
    Info(Info),
    /// How far counting a folder's contents has got.
    Folder(Tally),
    Full { image: Arc<ImageBuf>, size: u64 },
    Message(String),
    Scaled { scale_gen: u64, w: i32, h: i32, rgba: Vec<u8> },
    VideoStarted { remote: Remote, frames: Frames },
    VideoFrame { index: usize, w: u32, h: u32 },
    VideoState(PlayState),
    /// The player has no picture to show, but it's playing: this is what it's playing.
    Sound(Info),
    /// Text, or Markdown, ready to lay out.
    Doc { doc: Doc, info: Info },
    /// A PDF's page sizes, each to be drawn when it's scrolled to.
    Pdf { pages: Vec<tools::Page>, info: Info },
    /// One page of a PDF drawn this wide, or `None` if it couldn't be.
    PdfPage { index: usize, width: i32, image: Option<Arc<ImageBuf>> },
    /// A picture of the file that one of the system's thumbnailers made, and which one.
    Picture { image: Arc<ImageBuf>, by: String },
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
    /// Decodes `input` as an image, or as a document with `mode` (see `decode::TEXT_ARG`).
    fn spawn(input: File, mode: Option<&str>) -> std::io::Result<Self> {
        let output = memfd_create("gannet-preview", MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING)?;
        // `/proc/self/exe` rather than a path: this binary, even if it's been upgraded since.
        let mut command = Command::new("/proc/self/exe");
        command.arg(decode::ARG).args(mode).env_clear().current_dir("/").stdin(Stdio::from(input));
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
        Self::run(hub, generation, move |cancel, send| {
            let place = Place { path: &path, kinds: &kinds, cache_home: cache_home.as_deref() };
            fetch(&place, slow, video_size, cancel, send)
        })
    }

    /// Runs `work` on a thread of its own, passing on what it finds.
    fn run(hub: Hub, generation: u64, work: impl FnOnce(&OwnedFd, &dyn Fn(Found)) -> Option<Found> + Send + 'static) -> Option<Self> {
        let cancel = Arc::new(eventfd(0, EventfdFlags::CLOEXEC | EventfdFlags::NONBLOCK).ok()?);
        let theirs = cancel.clone();
        std::thread::Builder::new()
            .name("gannet-preview".into())
            .spawn(move || {
                if let Some(found) = work(&theirs, &|found| hub.send(Msg::Preview(Event { generation, found }))) {
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
    // Folders, pipes, sockets and devices are only described, never opened: a socket can't be,
    // and a device may not be yours to read.
    if let Ok(meta) = std::fs::metadata(path)
        && !meta.is_file()
    {
        let found = describe(at, &meta, None, None, cancel);
        if !meta.is_dir() {
            return Some(found);
        }
        send(found);
        let tally = foldersize::measure(path, &|| cancelled_within(cancel, Duration::ZERO), &mut |tally| send(Found::Folder(tally)))?;
        return Some(Found::Folder(tally));
    }
    // Non-blocking so a FIFO swapped in since can't hang this on open. It's turned away just below.
    let file = match File::options().read(true).custom_flags(libc::O_NONBLOCK).open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => return message("You don't have permission to read this file"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return message("This file is no longer here"),
        Err(_) => return message("This file can't be read"),
    };
    let Ok(meta) = file.metadata() else { return message("This file can't be read") };
    if !meta.is_file() {
        return Some(describe(at, &meta, None, None, cancel));
    }
    // Read where it is rather than with `read`, which would move the decoder's starting point.
    let mut head = vec![0u8; mime::HEAD];
    let head = file.read_at(&mut head, 0).ok().map(|n| head[..n].to_vec());
    // Text goes to the decoder as text: no image the decoder reads starts out looking like it. An
    // image that's written as text, like an SVG, is a picture if a thumbnailer can make one.
    let text = head.as_deref().filter(|h| is_text(at, h));
    if let Some(h) = text
        && let mime = at.kinds.db().guess(file_name(at.path), Some(h))
        && mime.starts_with("image/")
        && let Some(found) = picture(at, mime, video_size, cancel)
    {
        return Some(found);
    }
    let mode = text.map(|h| if is_markdown(at, h) { decode::MARKDOWN_ARG } else { decode::TEXT_ARG });
    let mut helper = match Helper::spawn(file, mode) {
        Ok(h) => h,
        Err(_) => return message("The previewer couldn't start"),
    };
    let mut waited = helper.wait(cancel, THUMBNAIL_AFTER);
    // The thumbnail, once it's been looked for: `Some(None)` when there isn't one.
    let mut looked: Option<Option<Arc<ImageBuf>>> = None;
    if slow.is_some() || matches!(waited, Waited::TimedOut) {
        let found = at.cache_home.and_then(|cache_home| thumbnail(cache_home, path, meta.mtime(), cancel)).map(Arc::new);
        if let Some(image) = &found {
            send(Found::Thumbnail(image.clone()));
        }
        looked = Some(found);
        if let Some(slow) = slow
            && cancelled_within(cancel, slow)
        {
            return None;
        }
        waited = helper.wait(cancel, DECODE_LIMIT);
    }
    let unsupported = || Some(describe(at, &meta, head.as_deref(), looked.clone(), cancel));
    if mode.is_some()
        && let Waited::Exited(status) = waited
    {
        // A zero byte past the part that was looked at means it isn't text after all.
        return match (status.code(), read_doc(&helper.output)) {
            (Some(0), Some(doc)) => match describe(at, &meta, head.as_deref(), Some(None), cancel) {
                Found::Info(info) => Some(Found::Doc { doc, info }),
                other => Some(other),
            },
            (Some(decode::EXIT_UNSUPPORTED), _) => unsupported(),
            _ => text_answer(status),
        };
    }
    match waited {
        Waited::Cancelled => None,
        Waited::TimedOut => message("This file is taking too long to read"),
        Waited::Exited(status) if status.code() == Some(decode::EXIT_UNSUPPORTED) && is_pdf(at, head.as_deref()) => open_pdf(at, cancel, &|| describe(at, &meta, head.as_deref(), looked.clone(), cancel)).or_else(|| other_picture(at, head.as_deref(), video_size, cancel)).or_else(unsupported),
        Waited::Exited(status) if status.code() == Some(decode::EXIT_UNSUPPORTED) && playable(at, head.as_deref()) => play(path, video_size, cancel, send, &|| describe(at, &meta, head.as_deref(), looked.clone(), cancel)),
        Waited::Exited(status) if status.code() == Some(decode::EXIT_UNSUPPORTED) => other_picture(at, head.as_deref(), video_size, cancel).or_else(unsupported),
        Waited::Exited(status) => answer(status, &helper.output, meta.len()).or_else(unsupported),
    }
}

/// The type, name, size and date of anything, and a picture of it if another program made one.
/// `looked` is the thumbnail if it's already been looked for.
fn describe(at: &Place, meta: &std::fs::Metadata, head: Option<&[u8]>, looked: Option<Option<Arc<ImageBuf>>>, cancel: &OwnedFd) -> Found {
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
    let thumbnail = match (looked, at.cache_home) {
        (Some(looked), _) => looked,
        (None, Some(cache_home)) if kind.is_file() => thumbnail(cache_home, at.path, meta.mtime(), cancel).map(Arc::new),
        _ => None,
    };
    let size = kind.is_file().then_some(meta.len());
    Found::Info(Info { mime: mime.to_string(), description, icon, size, modified: Some(meta.mtime()), thumbnail, folder: None })
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

/// Text, as far as the start of it shows: no zero byte, and either it reads as text, or the
/// database says it is, as for a Latin-1 file or a log with colour codes in it.
fn is_text(at: &Place, head: &[u8]) -> bool {
    // A PDF can start out all plain words, but it's still a PDF.
    if head.is_empty() || head.contains(&0) || head.starts_with(b"%PDF-") {
        return false;
    }
    if mime::looks_like_text(head) {
        return true;
    }
    let name = at.path.file_name().map_or(&b""[..], |n| n.as_encoded_bytes());
    at.kinds.db().guess(name, Some(head)).starts_with("text/")
}

/// What a decoder reading text ending other than with a document or "not text" means.
fn text_answer(status: ExitStatus) -> Option<Found> {
    match (status.code(), status.signal()) {
        (Some(decode::EXIT_NO_SANDBOX), _) => message(NO_SANDBOX),
        (_, Some(libc::SIGSYS)) => {
            eprintln!("gannet: the sandbox stopped a preview decoder");
            message(SANDBOX_STOPPED)
        }
        (_, Some(libc::SIGABRT | libc::SIGKILL | libc::SIGXFSZ)) | (Some(decode::EXIT_TOO_LARGE), _) => message(TEXT_TOO_LARGE),
        _ => message(DAMAGED_TEXT),
    }
}

fn file_name(path: &Path) -> &[u8] {
    path.file_name().map_or(&b""[..], |n| n.as_encoded_bytes())
}

/// The longest a program outside gets to make a picture or draw a page.
const TOOL_LIMIT: Duration = Duration::from_secs(30);
const LOCKED_PDF: &str = "This PDF is locked with a password";
const DAMAGED_PDF: &str = "This PDF can't be read. It may be damaged.";
const PAGE_FAILED: &str = "This page can't be drawn";
const SLOW_PDF: &str = "This PDF took too long to open";
const PDF_NO_BUBBLEWRAP: &str = "PDF previews need bubblewrap, which isn't installed";

fn is_pdf(at: &Place, head: Option<&[u8]>) -> bool {
    head.is_some_and(|h| h.starts_with(b"%PDF-")) || at.kinds.db().guess(file_name(at.path), head) == "application/pdf"
}

/// Opens the file again for a program outside, as long as it's still a file.
fn reopen(path: &Path) -> Option<File> {
    let file = File::options().read(true).custom_flags(libc::O_NONBLOCK).open(path).ok()?;
    file.metadata().is_ok_and(|m| m.is_file()).then_some(file)
}

/// Why a jailed program didn't get to end by itself.
enum NotRun {
    NoBubblewrap,
    /// It couldn't start, took too long or was cancelled.
    Stopped,
}

/// Runs a jailed program and waits for it: how it ended, and what it wrote to stdout.
fn run_tool(argv: &[OsString], input: File, bind: Option<&str>, cancel: &OwnedFd) -> Result<(ExitStatus, OwnedFd), NotRun> {
    let output = memfd_create("gannet-tool", MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING).map_err(|_| NotRun::Stopped)?;
    let command = tools::jailed(argv, input, bind).map_err(|_| NotRun::Stopped)?;
    let mut helper = Helper::run(command, output).map_err(|e| if e.kind() == std::io::ErrorKind::NotFound { NotRun::NoBubblewrap } else { NotRun::Stopped })?;
    match helper.wait(cancel, TOOL_LIMIT) {
        Waited::Exited(status) => Ok((status, helper.output.try_clone().map_err(|_| NotRun::Stopped)?)),
        _ => Err(NotRun::Stopped),
    }
}

/// Decodes a PNG another program wrote, in the decoder's sandbox like any other image.
fn decode_output(output: &OwnedFd, cancel: &OwnedFd) -> Option<ImageBuf> {
    rustix::fs::seek(output, rustix::fs::SeekFrom::Start(0)).ok()?;
    let mut helper = Helper::spawn(File::from(output.try_clone().ok()?), None).ok()?;
    match helper.wait(cancel, DECODE_LIMIT) {
        Waited::Exited(status) if status.success() => ImageBuf::from_memfd(&helper.output),
        _ => None,
    }
}

/// A picture of the file made by the system's thumbnailer for its type, the biggest that makes
/// sense for a window this size. `None` if there's no thumbnailer for it or it didn't work.
fn picture(at: &Place, mime: &str, area: (i32, i32), cancel: &OwnedFd) -> Option<Found> {
    let ext = tools::extension(at.path);
    let size = area.0.max(area.1).clamp(256, 2048) as u32;
    // Each one for the type that's installed where the jail can see it, until one works.
    for thumbnailer in at.kinds.thumbnailers().find_all(mime) {
        if cancelled_within(cancel, Duration::ZERO) {
            return None;
        }
        let Some(program) = thumbnailer.program(&[PathBuf::from(tools::PROGRAMS)]) else { continue };
        let Some(argv) = tools::thumbnailer(|input, output| thumbnailer.command(&program, input, output, size), &ext) else { continue };
        let Ok((status, output)) = run_tool(&argv, reopen(at.path)?, Some(&ext), cancel) else { continue };
        if !status.success() {
            continue;
        }
        let Some(image) = decode_output(&output, cancel) else { continue };
        let by = program.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        return Some(Found::Picture { image: Arc::new(image), by });
    }
    None
}

/// A picture of something nothing else here can show, by its type.
fn other_picture(at: &Place, head: Option<&[u8]>, area: (i32, i32), cancel: &OwnedFd) -> Option<Found> {
    let mime = at.kinds.db().guess(file_name(at.path), head);
    picture(at, mime, area, cancel)
}

/// A PDF's pages, ready to be drawn as they're scrolled to. `None` if poppler isn't installed.
fn open_pdf(at: &Place, cancel: &OwnedFd, describe: &dyn Fn() -> Found) -> Option<Found> {
    if !Path::new(tools::PROGRAMS).join("pdfinfo").is_file() || !Path::new(tools::PROGRAMS).join("pdftoppm").is_file() {
        return None;
    }
    let (status, out) = match run_tool(&tools::pdfinfo(), reopen(at.path)?, Some(tools::PDF_EXT), cancel) {
        Ok(ran) => ran,
        Err(NotRun::NoBubblewrap) => return message(PDF_NO_BUBBLEWRAP),
        Err(NotRun::Stopped) => return (!cancelled_within(cancel, Duration::ZERO)).then(|| Found::Message(SLOW_PDF.to_string())),
    };
    match tools::parse_pdfinfo(&String::from_utf8_lossy(&read_capped(&out)), status.success()) {
        tools::PdfInfo::Pages(pages) => match describe() {
            Found::Info(info) => Some(Found::Pdf { pages, info }),
            other => Some(other),
        },
        tools::PdfInfo::Locked => message(LOCKED_PDF),
        tools::PdfInfo::Unreadable => message(DAMAGED_PDF),
    }
}

/// What a program wrote, up to a size worth reading.
fn read_capped(output: &OwnedFd) -> Vec<u8> {
    let len = fstat(output).map_or(0, |s| s.st_size.max(0) as usize).min(1024 * 1024);
    let mut bytes = vec![0u8; len];
    let mut at = 0;
    while at < len {
        match rustix::io::pread(output, &mut bytes[at..], at as u64) {
            Ok(0) | Err(_) => break,
            Ok(n) => at += n,
        }
    }
    bytes.truncate(at);
    bytes
}

/// Draws one page of a PDF `width` pixels across.
fn pdf_page(path: &Path, index: usize, width: i32, turned: bool, cancel: &OwnedFd) -> Option<Found> {
    let failed = Some(Found::PdfPage { index, width, image: None });
    let Some(file) = reopen(path) else { return failed };
    let output = match run_tool(&tools::pdftoppm(index, width, turned), file, Some(tools::PDF_EXT), cancel) {
        Ok((status, output)) if status.success() => output,
        _ if cancelled_within(cancel, Duration::ZERO) => return None,
        _ => return failed,
    };
    match decode_output(&output, cancel) {
        Some(image) => Some(Found::PdfPage { index, width, image: Some(Arc::new(image)) }),
        None if cancelled_within(cancel, Duration::ZERO) => None,
        None => failed,
    }
}

/// Markdown, by its name or by what the database says it is.
fn is_markdown(at: &Place, head: &[u8]) -> bool {
    let name = at.path.file_name().map_or(&b""[..], |n| n.as_encoded_bytes());
    let lower = name.to_ascii_lowercase();
    // The database may be missing, and these are what Markdown is called.
    [&b".md"[..], b".markdown", b".mkd", b".mdown"].iter().any(|ext| lower.ends_with(ext)) || at.kinds.db().guess(name, Some(head)) == "text/markdown"
}

/// The document the decoder wrote, if it's all there and makes sense.
fn read_doc(output: &OwnedFd) -> Option<doc::Doc> {
    let len = usize::try_from(fstat(output).ok()?.st_size).ok()?;
    if len > doc::MAX_ENCODED {
        return None;
    }
    let mut bytes = vec![0u8; len];
    let mut at = 0;
    while at < len {
        match rustix::io::pread(output, &mut bytes[at..], at as u64) {
            Ok(0) | Err(_) => return None,
            Ok(n) => at += n,
        }
    }
    doc::decode(&bytes)
}

/// Whether the player should have a go at a file the image decoder doesn't know: one that starts
/// like a video, or that the database says is sound or video.
fn playable(at: &Place, head: Option<&[u8]>) -> bool {
    if head.is_some_and(media::looks_like_video) {
        return true;
    }
    let name = at.path.file_name().map_or(&b""[..], |n| n.as_encoded_bytes());
    let mime = at.kinds.db().guess(name, head);
    mime.starts_with("audio/") || mime.starts_with("video/")
}

/// Plays a video, or sound, until it's cancelled or something stops it. Returns the words to show
/// if so. A file with no picture plays with `otherwise`'s description of it in the picture's place.
fn play(path: &Path, size: (i32, i32), cancel: &OwnedFd, send: &dyn Fn(Found), otherwise: &dyn Fn() -> Found) -> Option<Found> {
    let Ok(video) = File::options().read(true).custom_flags(libc::O_NONBLOCK).open(path) else { return message("This file can't be read") };
    // It may have been swapped for something else since it was first looked at.
    if !video.metadata().is_ok_and(|m| m.is_file()) {
        return message("This file has changed. Look at it again to see what it is now.");
    }
    let started = match media::start_jailed(video, size, &[media::ARG], true) {
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
        Said::Sound => send(match otherwise() {
            Found::Info(info) => Found::Sound(info),
            other => other,
        }),
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
        let Ok(mut helper) = Helper::spawn(input, None) else { continue };
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
    Doc { doc: Doc, info: Info },
    Pdf(Pdf),
    Message(String),
}

/// A PDF being shown: its pages' sizes, and the ones drawn so far, at the width they were drawn.
pub struct Pdf {
    pub pages: Vec<tools::Page>,
    pub info: Info,
    /// The width and height it was last laid out at, to keep the same part in view at another.
    laid_out: Option<(i32, i32)>,
    pub drawn: HashMap<usize, (i32, Option<Arc<ImageBuf>>)>,
    /// The page being drawn now, at what width.
    drawing: Option<(usize, i32, Job)>,
}

#[cfg(test)]
impl Pdf {
    /// The page being drawn now, if one is.
    pub fn drawing_page(&self) -> Option<usize> {
        self.drawing.as_ref().map(|d| d.0)
    }
}

/// Where a PDF's pages go, for a window this wide.
struct PdfLayout {
    /// Each page's top and height.
    pages: Vec<(i32, i32)>,
    x: i32,
    width: i32,
    height: i32,
}

fn pdf_layout(sizes: &[tools::Page], area_w: i32, pad: i32, gap: i32) -> PdfLayout {
    let width = (area_w - pad * 2).max(1);
    let mut y = pad;
    let pages = sizes
        .iter()
        .map(|&tools::Page { w, h, .. }| {
            let height = ((width as f32 * h / w).round() as i32).clamp(1, width.saturating_mul(50));
            let slot = (y, height);
            y += height + gap;
            slot
        })
        .collect();
    PdfLayout { pages, x: pad, width, height: y - gap + pad }
}

/// A running player, and what it last said.
pub struct Video {
    remote: Remote,
    frames: Frames,
    pub frame: Option<(usize, u32, u32)>,
    pub state: PlayState,
    pub frames_shown: u64,
    /// Playing sound, with no picture: what's shown instead.
    pub sound: Option<Info>,
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
    /// The frame of an animation that's showing, and when the next one is due.
    pub frame: usize,
    frame_at: Option<Instant>,
    /// How far down a document is scrolled, in pixels, and where its lines go at this width.
    pub scroll: f64,
    pub doc_layout: Option<docview::Layout>,
    /// The thumbnailer that made the picture showing, when one did.
    pub made_by: Option<String>,
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
            path: path.clone(),
            content: Content::Waiting,
            size: None,
            zoom: None,
            pan: (0.0, 0.0),
            loading_shown: false,
            frame: 0,
            frame_at: None,
            scroll: 0.0,
            doc_layout: None,
            made_by: None,
            // Only the cursor's row is this file, and only once the listing has caught up.
            seen: if self.path_at_cursor().as_ref() == Some(&path) { self.stat_at_cursor() } else { None },
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
            // Asked for by name, so it's shown even if the cursor hasn't moved: the window may
            // have walked off somewhere the list doesn't follow.
            self.previewer_cursor = Some(path.clone());
            if self.previewer.as_ref() != Some(&path) {
                self.previewer = Some(path.clone());
                self.cmds.push(Cmd::PreviewShow(path));
            }
            return;
        }
        self.previewer_gen += 1;
        self.cmds.push(Cmd::OpenPreview { generation: self.previewer_gen, path: path.clone(), hidden: self.show_hidden, sort: self.sort });
        self.previewer = Some(path.clone());
        self.previewer_cursor = Some(path);
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
        self.preview_animate(now);
    }

    pub fn preview_deadline(&self) -> Option<Instant> {
        let p = self.preview.as_ref()?;
        [p.start_at, p.loading_at, p.frame_at].into_iter().flatten().min()
    }

    /// Moves an animation on to the frame that's due.
    fn preview_animate(&mut self, now: Instant) {
        let area = self.preview_geometry().area;
        let Some(p) = self.preview.as_mut() else { return };
        let (Some(due), Content::Image { image, .. }) = (p.frame_at, &p.content) else { return };
        if due > now || image.delays.is_empty() {
            return;
        }
        p.frame = (p.frame + 1) % image.delays.len();
        let next = due + image.delays[p.frame];
        // Falling well behind, as after the machine slept, starts the timing again from now
        // rather than racing through the frames it missed.
        p.frame_at = Some(if next < now { now + image.delays[p.frame] } else { next });
        self.invalidate(area);
    }

    pub fn on_preview(&mut self, e: Event) {
        let old = self.preview_rect();
        let Some(p) = self.preview.as_mut().filter(|p| p.generation == e.generation) else { return };
        match e.found {
            Found::Thumbnail(image) => {
                if matches!(p.content, Content::Waiting) {
                    p.content = Content::Image { image, thumbnail: true };
                    p.loading_at = None;
                }
            }
            Found::Full { image, size } => {
                p.frame = 0;
                p.frame_at = image.delays.first().map(|d| Instant::now() + *d);
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
                // A folder's contents are still being counted.
                if info.mime != mime::FOLDER {
                    p.job = None;
                }
                p.content = Content::Info(info);
                p.loading_at = None;
            }
            Found::Doc { doc, info } => {
                p.content = Content::Doc { doc, info };
                p.loading_at = None;
                p.job = None;
                if self.doc_fonts.is_none() {
                    self.doc_fonts = Some(docview::Fonts::new(&self.text, self.scale, self.colors.dark, self.cache_home.as_deref()));
                    let hub = self.hub.clone();
                    docview::find_fonts(self.cache_home.clone(), move |role, path, index| hub.send(Msg::DocFont(role, path, index)));
                }
            }
            Found::Picture { image, by } => {
                p.content = Content::Image { image, thumbnail: false };
                p.made_by = Some(by);
                p.loading_at = None;
                p.job = None;
                self.preview_rescale();
            }
            Found::Pdf { pages, info } => {
                p.content = Content::Pdf(Pdf { pages, info, laid_out: None, drawn: HashMap::new(), drawing: None });
                p.loading_at = None;
                p.job = None;
                self.pdf_draw_next();
            }
            Found::PdfPage { index, width, image } => {
                if let Content::Pdf(pdf) = &mut p.content {
                    if pdf.drawing.as_ref().is_some_and(|d| (d.0, d.1) == (index, width)) {
                        pdf.drawing = None;
                    }
                    pdf.drawn.insert(index, (width, image));
                }
                self.pdf_draw_next();
            }
            Found::Folder(tally) => {
                if let Content::Info(info) = &mut p.content {
                    info.folder = Some(tally);
                }
                if tally.done {
                    p.job = None;
                }
            }
            Found::VideoStarted { remote, frames } => {
                let area = self.preview_geometry().area;
                let Some(p) = self.preview.as_mut() else { return };
                let mut remote = remote;
                // The window may have changed since the player was started.
                remote.set_size(area.w.max(1), area.h.max(1));
                // What it starts as, so a key pressed before it first reports does the right thing.
                let state = PlayState { volume: 100.0, ..PlayState::default() };
                p.content = Content::Video(Video { remote, frames, frame: None, state, frames_shown: 0, sound: None, settled_at: Instant::now() });
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
            Found::Sound(info) => {
                if let Content::Video(video) = &mut p.content {
                    video.sound = Some(info);
                    p.loading_at = None;
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
        // A new width wants the pages in view drawn again to fit it.
        self.pdf_draw_next();
        let area = self.preview_geometry().area;
        if let Some(Preview { content: Content::Video(video), .. }) = self.preview.as_mut() {
            video.remote.set_size(area.w.max(1), area.h.max(1));
        }
        let Some(view) = self.preview_view() else { return };
        let hub = self.hub.clone();
        let Some(p) = self.preview.as_mut() else { return };
        let Content::Image { image, thumbnail: false } = &p.content else { return };
        let wanted = (view.dest.w, view.dest.h);
        // A smooth copy of every frame of an animation would cost far more than it's worth.
        let smooth = worth_smoothing(&view) && image.frames() == 1;
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
        if self.preview_video_key(sym, m) || self.preview_doc_key(sym, m) {
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
        let pdf_shown = self.pdf_page_shown();
        let open = self.info_open_rect();
        let Some(mut p) = self.preview.take() else { return };
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
                        _ => c.image(image.frame(p.frame), image.w as i32, image.h as i32, view.dest),
                    });
                }
                (Content::Video(Video { frame: Some((index, w, h)), frames, .. }), _) => {
                    let (scale, fit) = image_scale((area.w, area.h), *w, *h, true, None);
                    let dest = place(area, *w, *h, scale, fit, (0.0, 0.0)).dest;
                    App::with_clip(c, area, |c| c.xrgb(frames.frame(*index, *w, *h), *w as i32, *h as i32, dest));
                }
                (Content::Video(Video { sound: Some(info), .. }), _) => self.paint_info(c, area, info, None),
                // Drawn below, as laying it out changes the preview.
                (Content::Doc { .. }, _) => {}
                (Content::Message(text), _) => self.paint_center(c, area, text, co.muted),
                (Content::Info(info), _) => self.paint_info(c, area, info, open),
                _ if p.loading_shown => self.paint_center(c, area, "Loading…", co.muted),
                _ => {}
            }
            if matches!(p.content, Content::Doc { .. }) {
                self.paint_doc(c, area, &mut p);
            }
            if matches!(p.content, Content::Pdf(_)) {
                self.paint_pdf(c, area, &mut p);
            }
            let foot = match (&p.content, &view) {
                (Content::Image { image, thumbnail: false }, Some(view)) => {
                    let size = p.size.map_or_else(String::new, |s| format!("  ·  {}", format::size(s)));
                    let by = p.made_by.as_ref().map_or_else(String::new, |by| format!("  ·  a picture by {by}"));
                    format!("{} × {}{size}  ·  {:.0}%{by}", image.w, image.h, view.scale * 100.0)
                }
                (Content::Pdf(pdf), _) => {
                    let (shown, of) = pdf_shown.unwrap_or((1, pdf.pages.len()));
                    [Some(pdf.info.description.clone()), pdf.info.size.map(format::size), Some(format!("page {shown} of {of}"))].into_iter().flatten().collect::<Vec<_>>().join("  ·  ")
                }
                (Content::Image { thumbnail: true, .. }, _) => "Loading the full image…".to_string(),
                (Content::Info(info), _) => info.mime.clone(),
                (Content::Doc { doc, info }, _) => {
                    let lines = doc.lines();
                    let lines = (lines > 0).then(|| format::count(lines, "line", "lines"));
                    let cut = doc.cut.then(|| format!("showing the first {}", format::size(doc::TEXT_LIMIT as u64)));
                    [Some(info.description.clone()), info.size.map(format::size), lines, cut].into_iter().flatten().collect::<Vec<_>>().join("  ·  ")
                }
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

impl App {
    /// Text or Markdown, laid out again if the window's width or the fonts have changed.
    fn paint_doc(&mut self, c: &mut Canvas, area: Rect, p: &mut Preview) {
        let Content::Doc { doc, .. } = &p.content else { return };
        let Some(fonts) = self.doc_fonts.as_mut() else { return };
        if !p.doc_layout.as_ref().is_some_and(|l| l.fits(area.w, fonts)) {
            // Keeps the same part of it in view, as near as can be.
            // A scroll from before the first layout stands, and is cut to fit below.
            let along = p.doc_layout.as_ref().map(|l| p.scroll / l.height.max(1) as f64);
            let layout = docview::lay_out(doc, area.w, fonts);
            if let Some(along) = along {
                p.scroll = along * layout.height as f64;
            }
            p.doc_layout = Some(layout);
        }
        let Some(layout) = &p.doc_layout else { return };
        p.scroll = p.scroll.clamp(0.0, (layout.height - area.h).max(0) as f64);
        let co = self.colors;
        let ink = docview::Ink { text: co.foreground, muted: co.muted, link: co.accent, code: co.surface, rule: co.border };
        App::with_clip(c, area, |c| docview::paint(c, area, p.scroll as i32, layout, fonts, ink));
    }

    /// Scrolls a document by this much, if one's showing.
    fn doc_scroll_by(&mut self, pixels: f64) {
        let area = self.preview_geometry().area;
        let pdf_height = self.pdf_layout().map(|l| l.height);
        let Some(p) = self.preview.as_mut() else { return };
        // Until a document's been laid out, painting stops it at the end.
        let height = match &p.content {
            Content::Doc { .. } => p.doc_layout.as_ref().map_or(i32::MAX, |l| l.height),
            Content::Pdf(_) => pdf_height.unwrap_or(0),
            _ => return,
        };
        let most = height.saturating_sub(area.h).max(0) as f64;
        let next = (p.scroll + pixels).clamp(0.0, most);
        let pdf = matches!(p.content, Content::Pdf(_));
        if next != p.scroll {
            p.scroll = next;
            self.invalidate(area);
            // A PDF's footer says which page is in view, so it changes as it scrolls too.
            if pdf {
                let frame = self.preview_geometry();
                self.invalidate(Rect::new(frame.rect.x, area.bottom(), frame.rect.w, frame.rect.bottom() - area.bottom()));
            }
            self.pdf_draw_next();
        }
    }

    /// Where a PDF's pages go in the window as it is, if one's showing.
    fn pdf_layout(&self) -> Option<PdfLayout> {
        let Content::Pdf(pdf) = &self.preview.as_ref()?.content else { return None };
        Some(pdf_layout(&pdf.pages, self.preview_geometry().area.w, self.px(16.0), self.px(12.0)))
    }

    /// Draws the next page that's in view, or nearly, and isn't drawn at this width, and lets go of
    /// pages well out of view. One page is drawn at a time, those in view first.
    pub fn pdf_draw_next(&mut self) {
        let Some(layout) = self.pdf_layout() else { return };
        let area = self.preview_geometry().area;
        let hub = self.hub.clone();
        let Some(p) = self.preview.as_mut() else { return };
        let Content::Pdf(pdf) = &mut p.content else { return };
        // At a new width the same part stays in view, and the scroll is cut to fit before it's
        // used, or a window that grew would find nothing in view and let every page go.
        if let Some((width, height)) = pdf.laid_out
            && width != layout.width
        {
            p.scroll *= layout.height as f64 / height.max(1) as f64;
        }
        pdf.laid_out = Some((layout.width, layout.height));
        p.scroll = p.scroll.clamp(0.0, (layout.height - area.h).max(0) as f64);
        let (generation, path, scroll) = (p.generation, p.path.clone(), p.scroll as i32);
        let Content::Pdf(pdf) = &mut p.content else { return };
        let in_view = |(top, h): (i32, i32), reach: i32| top + h > scroll - reach && top < scroll + area.h + reach;
        let visible: Vec<usize> = (0..layout.pages.len()).filter(|&i| in_view(layout.pages[i], 0)).collect();
        // Half a window either way, so the next page is ready when it scrolls in.
        let near: Vec<usize> = (0..layout.pages.len()).filter(|&i| in_view(layout.pages[i], area.h / 2)).collect();
        let keep = |i: usize| near.first().is_some_and(|&f| i + 2 >= f) && near.last().is_some_and(|&l| i <= l + 2);
        pdf.drawn.retain(|&i, _| keep(i));
        let wanted = visible.iter().chain(near.iter()).copied().find(|i| pdf.drawn.get(i).is_none_or(|d| d.0 != layout.width));
        match (&pdf.drawing, wanted) {
            (Some(d), Some(i)) if (d.0, d.1) == (i, layout.width) => {}
            // What's being drawn is still wanted, just not first: let it finish.
            (Some(d), _) if d.1 == layout.width && keep(d.0) && wanted.is_none_or(|i| !visible.contains(&i) || visible.contains(&d.0)) => {}
            (_, Some(i)) => {
                let (width, turned) = (layout.width, pdf.pages[i].turned);
                pdf.drawing = Job::run(hub, generation, move |cancel, _| pdf_page(&path, i, width, turned, cancel)).map(|job| (i, width, job));
            }
            (_, None) => pdf.drawing = None,
        }
    }

    /// The pages in view: each drawn one, scaled if it was drawn at another width, and a blank page
    /// for each still to come.
    fn paint_pdf(&mut self, c: &mut Canvas, area: Rect, p: &mut Preview) {
        let Content::Pdf(pdf) = &p.content else { return };
        let layout = pdf_layout(&pdf.pages, area.w, self.px(16.0), self.px(12.0));
        p.scroll = p.scroll.clamp(0.0, (layout.height - area.h).max(0) as f64);
        let co = self.colors;
        let top = area.y - p.scroll as i32;
        for (i, &(y, h)) in layout.pages.iter().enumerate() {
            let page = Rect::new(area.x + layout.x, top + y, layout.width, h);
            if page.bottom() < area.y || page.y > area.bottom() {
                continue;
            }
            App::with_clip(c, area, |c| match pdf.drawn.get(&i) {
                Some((_, Some(image))) => c.image(image.pixels(), image.w as i32, image.h as i32, page),
                _ => c.fill(page, 0xffffff),
            });
            if !matches!(pdf.drawn.get(&i), Some((_, Some(_)))) {
                let words = if matches!(pdf.drawn.get(&i), Some((_, None))) { PAGE_FAILED.to_string() } else { format!("Page {}", i + 1) };
                let clipped = page.intersect(area);
                if !clipped.is_empty() {
                    App::with_clip(c, clipped, |c| self.paint_center(c, page, &words, co.muted));
                }
            }
        }
    }

    /// The page at the top of the view, from 1, and how many there are.
    fn pdf_page_shown(&self) -> Option<(usize, usize)> {
        let layout = self.pdf_layout()?;
        let scroll = self.preview.as_ref()?.scroll as i32 + self.preview_geometry().area.h / 3;
        let shown = layout.pages.iter().position(|&(y, h)| y + h > scroll).unwrap_or(layout.pages.len().saturating_sub(1));
        Some((shown + 1, layout.pages.len()))
    }

    fn doc_line(&mut self) -> f64 {
        let fallback = self.px(20.0) as f64;
        self.doc_fonts.as_mut().map_or(fallback, docview::Layout::line_height)
    }

    /// The wheel: scrolls a document. Nothing else in the preview scrolls.
    pub fn preview_scroll(&mut self, lines: f64, pixels: f64) {
        let by = lines * 3.0 * self.doc_line() + pixels * self.scale;
        self.doc_scroll_by(by);
    }

    /// Keys that move through a document. Arrows without Shift still go to other files.
    fn preview_doc_key(&mut self, sym: u32, m: Mods) -> bool {
        if !self.preview.as_ref().is_some_and(|p| matches!(p.content, Content::Doc { .. } | Content::Pdf(_))) || m.ctrl {
            return false;
        }
        let page = (self.preview_geometry().area.h as f64 - self.doc_line() * 2.0).max(self.doc_line());
        let line = self.doc_line();
        match sym {
            k::KEY_Page_Down => self.doc_scroll_by(page),
            k::KEY_Page_Up => self.doc_scroll_by(-page),
            k::KEY_Down if m.shift => self.doc_scroll_by(line),
            k::KEY_Up if m.shift => self.doc_scroll_by(-line),
            k::KEY_Home => self.doc_scroll_by(f64::MIN),
            k::KEY_End => self.doc_scroll_by(f64::MAX),
            _ => return false,
        }
        true
    }
}

/// A file's name as it's shown, or its whole path if it has none.
fn display_name(path: &Path) -> String {
    path.file_name().map_or_else(|| path.to_string_lossy().into_owned(), |n| n.to_string_lossy().into_owned())
}

/// What a folder holds, as far as it's been counted.
fn folder_contents(tally: Option<Tally>) -> String {
    let Some(t) = tally else { return "Counting…".to_string() };
    let items = format::count(t.items as usize, "item", "items");
    let bytes = format::size(t.bytes);
    match (t.done, t.incomplete) {
        (false, _) => format!("{items}  ·  {bytes} so far…"),
        (true, false) => format!("{items}  ·  {bytes} in all"),
        // Some of it couldn't be read, so the real total is more.
        (true, true) => format!("{items}  ·  at least {bytes}"),
    }
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
        let contents = (info.mime == mime::FOLDER).then(|| folder_contents(info.folder));
        let details: Vec<String> = [contents, info.size.map(format::size), info.modified.map(|m| format!("Modified {}", format::date(m, now)))].into_iter().flatten().collect();
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
        if self.previewer_cursor.as_ref() != Some(&path) {
            self.previewer_cursor = Some(path.clone());
            if self.previewer.as_ref() != Some(&path) {
                self.previewer = Some(path.clone());
                self.cmds.push(Cmd::PreviewShow(path));
            }
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
        // A file the list doesn't show (its filter hides it, say) leaves the cursor where it is.
        let Some(row) = self.listing.as_ref().and_then(|l| self.rows.iter().position(|&i| *l.key(i) == *name)) else { return };
        if row != self.cursor || !self.cursor_shown {
            self.move_cursor(row, Mods { ctrl: self.sel_count > 1, ..Mods::default() });
        }
        self.previewer_cursor = Some(path);
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

    #[test]
    fn sockets_and_folders_are_described_without_being_opened() {
        assert!(std::env::var_os("GANNET_TEST_SANDBOX").is_some(), "run tests with scripts/test.sh");
        let dir = std::env::temp_dir().join(format!("gannet-test-describe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("folder/inner")).unwrap();
        std::fs::write(dir.join("folder/inner/a"), [0; 1234]).unwrap();
        let _socket = std::os::unix::net::UnixListener::bind(dir.join("socket")).unwrap();
        let kinds = Kinds::default();
        let cancel = event();
        let socket = dir.join("socket");
        let at = Place { path: &socket, kinds: &kinds, cache_home: None };
        match fetch(&at, None, (100, 100), &cancel, &|_| {}) {
            Some(Found::Info(info)) => assert_eq!(info.mime, "inode/socket"),
            Some(Found::Message(m)) => panic!("socket: {m}"),
            _ => panic!("socket: no answer"),
        }
        // A folder is described first, then counted.
        let folder = dir.join("folder");
        let at = Place { path: &folder, kinds: &kinds, cache_home: None };
        let sent = std::cell::RefCell::new(Vec::new());
        match fetch(&at, None, (100, 100), &cancel, &|found| sent.borrow_mut().push(found)) {
            Some(Found::Folder(tally)) => assert_eq!((tally.items, tally.bytes, tally.done), (1, 1234, true)),
            _ => panic!("folder: no count"),
        }
        assert!(matches!(sent.borrow().first(), Some(Found::Info(info)) if info.mime == mime::FOLDER));
        std::fs::remove_dir_all(&dir).unwrap();
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

    fn animation(w: u32, h: u32, frames: &[u8], delays: &[u32], count: u32) -> Vec<u8> {
        let mut out = decode::ANIMATION_MAGIC.to_vec();
        for word in [w, h] {
            out.extend_from_slice(&word.to_ne_bytes());
        }
        out.extend_from_slice(frames);
        for word in delays.iter().chain([&count]) {
            out.extend_from_slice(&word.to_ne_bytes());
        }
        out
    }

    #[test]
    fn reads_an_animation_and_its_timing() {
        let image = ImageBuf::from_memfd(&memfd_with(&animation(1, 1, &[1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3], &[50, 0, 250], 3))).unwrap();
        assert_eq!(image.frames(), 3);
        assert_eq!((image.frame(0), image.frame(2)), (&[1u8; 4][..], &[3u8; 4][..]));
        assert_eq!(image.pixels(), image.frame(0));
        // Next to no time reads as a tenth of a second, as in browsers.
        assert_eq!(image.delays, [Duration::from_millis(50), Duration::from_millis(100), Duration::from_millis(250)]);
        assert_eq!(image.frame(99), image.frame(2), "past the end is the last frame, not past the mapping");
    }

    #[test]
    fn doesnt_believe_a_bad_animation() {
        // A count that doesn't match, one frame, too many, and a count off the end.
        assert!(ImageBuf::from_memfd(&memfd_with(&animation(1, 1, &[0; 8], &[10, 10], 3))).is_none());
        assert!(ImageBuf::from_memfd(&memfd_with(&animation(1, 1, &[0; 4], &[10], 1))).is_none());
        assert!(ImageBuf::from_memfd(&memfd_with(&animation(1, 1, &[0; 8], &[10, 10], decode::MAX_FRAMES + 1))).is_none());
        assert!(ImageBuf::from_memfd(&memfd_with(&animation(u32::MAX, u32::MAX, &[0; 8], &[10, 10], 2))).is_none());
        assert!(ImageBuf::from_memfd(&memfd_with(&decode::ANIMATION_MAGIC[..].repeat(3))).is_none());
    }

    #[test]
    fn text_is_known_by_a_zero_byte_or_by_its_type() {
        let kinds = Kinds::new(vec![PathBuf::from("/usr/share/mime")], None);
        let at = |name: &str| PathBuf::from("/nowhere").join(name);
        let is = |name: &str, head: &[u8]| is_text(&Place { path: &at(name), kinds: &kinds, cache_home: None }, head);
        assert!(is("notes", b"plain words\n"));
        // Not GLib's idea of text, but text all the same.
        assert!(is("build.log", b"\x1b[31merror\x1b[0m: it broke\n"), "colour codes in a log");
        assert!(is("cafe.txt", b"caf\xe9 au lait\n"), "Latin-1");
        assert!(!is("data.bin", b"\x89\xfe\x01\x02 no zero byte here"), "bytes the database doesn't call text");
        assert!(!is("notes.txt", b"text\0then a zero"));
        assert!(!is("plain.pdf", b"%PDF-1.4\n1 0 obj << /Type /Catalog >> endobj\n"), "a PDF written all in plain words");
        assert!(!is("empty.txt", b""));
    }

    #[test]
    fn a_text_decoder_ending_badly_says_so_in_words_about_text() {
        let said = |raw: i32| match text_answer(ExitStatus::from_raw(raw)) {
            Some(Found::Message(m)) => m,
            _ => panic!("no message for {raw}"),
        };
        assert_eq!(said(libc::SIGKILL), TEXT_TOO_LARGE, "a limit");
        assert_eq!(said(decode::EXIT_TOO_LARGE << 8), TEXT_TOO_LARGE);
        assert_eq!(said(libc::SIGSYS), SANDBOX_STOPPED);
        assert_eq!(said(decode::EXIT_NO_SANDBOX << 8), NO_SANDBOX);
        assert_eq!(said(decode::EXIT_DAMAGED << 8), DAMAGED_TEXT);
        assert!(!said(libc::SIGKILL).contains("image"));
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
