//! Thumbnails in place of the icons of the list's pictures and videos.
//!
//! Only the rows on screen are asked for, and a row scrolled past before its turn is dropped
//! from the queue, or stopped if it's being made. One thread looks in the freedesktop cache,
//! which other programs share, and two more make what isn't there, so a slow video never holds
//! up a thumbnail that's ready. They run at the lowest CPU priority and a low disk one, as do
//! the programs they start.
//!
//! What's made is written to the cache at the "normal" size, and a file that can't be made into
//! one is noted in the cache's failures folder, so it isn't tried again until it changes. Nothing
//! here decodes: pictures go to the sandboxed decoder (see `decode`), and videos to `ffmpeg` in
//! the jail the preview's programs run in (see `tools`).

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use gannet_core::listing::Kind;
use gannet_core::{mime, thumbcache};
use rustix::event::{EventfdFlags, eventfd};
use rustix::fs::{MemfdFlags, SeekFrom, memfd_create, seek};

use crate::app::{Hub, Msg};
use crate::decode;
use crate::icons::{Icon, icon_for_name};
use crate::preview::{self, Helper, ImageBuf, Kinds, NotRun, Waited};
use crate::tools;

/// How many thumbnails are made at once. Each is a process that can take a core.
const MAKERS: usize = 2;
/// Rows' pictures kept, which is many screens of them. Each is a few kilobytes.
const KEEP: usize = 512;
/// A cached thumbnail is small, so taking longer than this means something's wrong with it.
const READ_LIMIT: Duration = Duration::from_secs(10);
const MAKE_LIMIT: Duration = Duration::from_secs(30);
/// What a failure note says made it. A newer Gannet may manage what this one couldn't, so it
/// only believes its own.
const SOFTWARE: &str = concat!("Gannet ", env!("CARGO_PKG_VERSION"));

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Media {
    Image,
    Video,
}

/// Whether a row could have a thumbnail, and of what. Links are followed, as other file managers
/// do. Only files: a folder keeps its icon.
pub fn media(kind: Kind, name: &[u8]) -> Option<Media> {
    if !matches!(kind, Kind::File | Kind::Link) {
        return None;
    }
    match icon_for_name(name) {
        Icon::Image => Some(Media::Image),
        Icon::Video => Some(Media::Video),
        _ => None,
    }
}

/// A row's picture: the middle of the thumbnail, square, at most the row's picture size.
pub struct Pic {
    pub w: u32,
    pub h: u32,
    pub rgba: Vec<u8>,
}

pub enum Outcome {
    Shown(Arc<Pic>),
    /// There's no thumbnail to be had, so the row keeps its icon.
    Nothing,
    /// It was scrolled away while being made.
    Stopped,
}

pub struct Done {
    pub path: PathBuf,
    /// The picture size it was asked for.
    pub side: u32,
    pub outcome: Outcome,
}

struct Job {
    path: PathBuf,
    media: Media,
    side: u32,
}

#[derive(Default)]
struct Queue {
    /// Waiting for a look in the cache, top row first.
    reading: VecDeque<Job>,
    /// Not in the cache, waiting to be made.
    making: VecDeque<Job>,
    /// Being made now, and how to stop each.
    running: Vec<(PathBuf, Arc<OwnedFd>)>,
    /// The rows on screen, for a look in the cache that finishes after its row has gone.
    on_screen: HashSet<PathBuf>,
}

#[derive(Default)]
struct Shared {
    queue: Mutex<Queue>,
    more: Condvar,
}

enum State {
    Asked,
    Shown(Arc<Pic>),
    Nothing,
}

struct Known {
    /// The file's modified time when it was asked for, if the listing knew it. Another time
    /// means it's changed since, and is asked for again.
    asked_for: Option<i64>,
    state: State,
}

/// The list's side: what's been asked for, and the pictures that have come back.
pub struct Thumbs {
    shared: Arc<Shared>,
    started: bool,
    /// How many making threads are running.
    makers: usize,
    hub: Hub,
    cache_home: PathBuf,
    kinds: Arc<Kinds>,
    known: HashMap<PathBuf, Known>,
    /// The order things became known, oldest first, for forgetting the oldest.
    order: VecDeque<PathBuf>,
    side: u32,
}

impl Thumbs {
    /// Nothing starts until a picture or a video is on screen.
    pub fn new(hub: Hub, cache_home: PathBuf, kinds: Arc<Kinds>) -> Self {
        Self { shared: Arc::default(), started: false, makers: 0, hub, cache_home, kinds, known: HashMap::new(), order: VecDeque::new(), side: 0 }
    }

    /// The rows on screen that could have a thumbnail, top first, with their modified times if
    /// the listing has them, at `side` pixels. Anything else that's waiting is dropped.
    pub fn want(&mut self, visible: &[(PathBuf, Media, Option<i64>)], side: u32) {
        if self.plan(visible, side) {
            self.start();
            self.shared.more.notify_all();
        }
    }

    /// The bookkeeping for `want`. Says whether anything new was queued.
    fn plan(&mut self, visible: &[(PathBuf, Media, Option<i64>)], side: u32) -> bool {
        if side != self.side {
            // Pictures for another size: each is asked for again, which the cache answers.
            self.side = side;
            self.known.clear();
            self.order.clear();
        }
        let mut guard = self.shared.queue.lock().unwrap_or_else(|e| e.into_inner());
        let queue = &mut *guard;
        if visible.is_empty() && queue.on_screen.is_empty() {
            return false;
        }
        queue.on_screen = visible.iter().map(|v| v.0.clone()).collect();
        let on_screen = &queue.on_screen;
        let mut dropped = Vec::new();
        for lane in [&mut queue.reading, &mut queue.making] {
            lane.retain(|job| {
                let keep = on_screen.contains(&job.path) && job.side == side;
                if !keep {
                    dropped.push(job.path.clone());
                }
                keep
            });
        }
        for (path, stop) in &queue.running {
            if !on_screen.contains(path) {
                let _ = rustix::io::write(&**stop, &1u64.to_ne_bytes());
            }
        }
        for path in dropped {
            if self.known.get(&path).is_some_and(|k| matches!(k.state, State::Asked)) {
                self.known.remove(&path);
            }
        }
        let mut queued = false;
        for (path, media, mtime) in visible {
            if let Some(k) = self.known.get_mut(path) {
                match (k.asked_for, mtime) {
                    _ if matches!(k.state, State::Asked) => continue,
                    (_, None) => continue,
                    (a, Some(m)) if a == Some(*m) => continue,
                    // The listing has only just found its time: what came back is for this one.
                    (None, Some(m)) => {
                        k.asked_for = Some(*m);
                        continue;
                    }
                    _ => {}
                }
            } else if !self.order.contains(path) {
                // One stopped or dropped is forgotten but can still be in the order.
                self.order.push_back(path.clone());
            }
            self.known.insert(path.clone(), Known { asked_for: *mtime, state: State::Asked });
            queue.reading.push_back(Job { path: path.clone(), media: *media, side });
            queued = true;
        }
        drop(guard);
        self.forget_oldest();
        queued
    }

    fn forget_oldest(&mut self) {
        // Each one still being asked for goes to the back, so this ends.
        for _ in 0..self.order.len() {
            if self.known.len() <= KEEP {
                break;
            }
            let Some(path) = self.order.pop_front() else { break };
            match self.known.get(&path) {
                Some(k) if matches!(k.state, State::Asked) => self.order.push_back(path),
                Some(_) => {
                    self.known.remove(&path);
                }
                None => {}
            }
        }
        self.order.retain(|p| self.known.contains_key(p));
    }

    /// Takes in a thumbnail that's come back. Says whether a row should be drawn again.
    pub fn on_done(&mut self, done: Done) -> bool {
        if done.side != self.side {
            return false;
        }
        let Some(k) = self.known.get_mut(&done.path) else { return false };
        if !matches!(k.state, State::Asked) {
            return false;
        }
        match done.outcome {
            Outcome::Shown(pic) => k.state = State::Shown(pic),
            Outcome::Nothing => k.state = State::Nothing,
            Outcome::Stopped => {
                self.known.remove(&done.path);
                return false;
            }
        }
        true
    }

    /// The picture to draw for a row, if it has one.
    pub fn picture(&self, path: &Path) -> Option<&Arc<Pic>> {
        match &self.known.get(path)?.state {
            State::Shown(pic) => Some(pic),
            _ => None,
        }
    }

    /// How many are still being looked for or made.
    #[cfg(any(test, feature = "bench"))]
    pub fn waiting(&self) -> usize {
        self.known.values().filter(|k| matches!(k.state, State::Asked)).count()
    }

    fn start(&mut self) {
        if self.started {
            return;
        }
        let cx = Arc::new(Cx { cache_home: self.cache_home.clone(), kinds: self.kinds.clone() });
        // Makers first: the reader passes them what isn't in the cache, so it needs one at least.
        while self.makers < MAKERS {
            let (shared, hub, cx) = (self.shared.clone(), self.hub.clone(), cx.clone());
            if std::thread::Builder::new().name("gannet-thumbs".into()).spawn(move || make_loop(&shared, &hub, &cx)).is_err() {
                break;
            }
            self.makers += 1;
        }
        let (shared, hub, reader_cx) = (self.shared.clone(), self.hub.clone(), cx.clone());
        if self.makers == 0 || std::thread::Builder::new().name("gannet-thumbs".into()).spawn(move || read_loop(&shared, &hub, &reader_cx)).is_err() {
            // Out of threads: nothing would answer, so what was asked for is let go, to be asked
            // for again, and starting is tried again then.
            let dropped: Vec<PathBuf> = self.shared.queue.lock().unwrap_or_else(|e| e.into_inner()).reading.drain(..).map(|job| job.path).collect();
            for path in dropped {
                self.known.remove(&path);
            }
            return;
        }
        self.started = true;
    }
}

/// What the threads need to know.
struct Cx {
    cache_home: PathBuf,
    kinds: Arc<Kinds>,
}

/// So a game or a build on the same computer comes first, and the programs these threads start
/// inherit it.
fn lower_priority() {
    const IOPRIO_WHO_PROCESS: libc::c_int = 1;
    const IOPRIO_CLASS_BE: libc::c_int = 2;
    // SAFETY: plain integers, about this thread only.
    unsafe {
        let me = libc::gettid();
        libc::setpriority(libc::PRIO_PROCESS, me as libc::id_t, 19);
        libc::syscall(libc::SYS_ioprio_set, IOPRIO_WHO_PROCESS, me, IOPRIO_CLASS_BE << 13 | 7);
    }
}

fn read_loop(shared: &Shared, hub: &Hub, cx: &Cx) {
    lower_priority();
    // Looking in the cache isn't stopped part way: a cached thumbnail is small, and one that
    // takes more than `READ_LIMIT` is given up on.
    let never = eventfd(0, EventfdFlags::CLOEXEC).ok();
    loop {
        let job = {
            let mut queue = shared.queue.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if let Some(job) = queue.reading.pop_front() {
                    break job;
                }
                queue = shared.more.wait(queue).unwrap_or_else(|e| e.into_inner());
            }
        };
        // Without one, every row is answered, so none waits forever.
        let looked = match &never {
            Some(never) => look(&job, cx, never),
            None => Some(Outcome::Nothing),
        };
        match looked {
            Some(outcome) => hub.send(Msg::Thumbnail(Done { path: job.path, side: job.side, outcome })),
            None => {
                let mut queue = shared.queue.lock().unwrap_or_else(|e| e.into_inner());
                if queue.on_screen.contains(&job.path) {
                    queue.making.push_back(job);
                    drop(queue);
                    shared.more.notify_all();
                } else {
                    // Scrolled away while it was looked for: it's asked for again if it's back.
                    drop(queue);
                    hub.send(Msg::Thumbnail(Done { path: job.path, side: job.side, outcome: Outcome::Stopped }));
                }
            }
        }
    }
}

fn make_loop(shared: &Shared, hub: &Hub, cx: &Cx) {
    lower_priority();
    loop {
        let (job, stop) = {
            let mut queue = shared.queue.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if let Some(job) = queue.making.pop_front() {
                    let stop = eventfd(0, EventfdFlags::CLOEXEC | EventfdFlags::NONBLOCK).ok().map(Arc::new);
                    if let Some(stop) = &stop {
                        queue.running.push((job.path.clone(), stop.clone()));
                    }
                    break (job, stop);
                }
                queue = shared.more.wait(queue).unwrap_or_else(|e| e.into_inner());
            }
        };
        // Out of descriptors: answered anyway, so the row doesn't wait forever.
        let Some(stop) = stop else {
            hub.send(Msg::Thumbnail(Done { path: job.path, side: job.side, outcome: Outcome::Nothing }));
            continue;
        };
        let outcome = make(&job, cx, &stop);
        shared.queue.lock().unwrap_or_else(|e| e.into_inner()).running.retain(|(_, s)| !Arc::ptr_eq(s, &stop));
        hub.send(Msg::Thumbnail(Done { path: job.path, side: job.side, outcome }));
    }
}

/// The answer from the cache: a thumbnail of the file as it is now, or a note that one
/// couldn't be made of it. `None` if it has to be made.
fn look(job: &Job, cx: &Cx, cancel: &OwnedFd) -> Option<Outcome> {
    let Ok(meta) = std::fs::metadata(&job.path) else { return Some(Outcome::Nothing) };
    if !meta.is_file() {
        return Some(Outcome::Nothing);
    }
    if thumbcache::is_inside(&cx.cache_home, &job.path) {
        return None;
    }
    let mtime = meta.mtime();
    // The smallest first: it's quickest to read, and the row needs no more.
    for candidate in thumbcache::candidates(&cx.cache_home, &job.path).iter().rev() {
        let Some(bytes) = read_small(candidate) else { continue };
        if thumbcache::recorded_mtime(&bytes) != Some(mtime) {
            continue;
        }
        // From a copy, so it can't be swapped for something else between the check and the decode.
        let Some(copy) = in_memory(&bytes) else { continue };
        match decode(copy, cancel, READ_LIMIT) {
            Ok(image) => return Some(shown(&image, job.side)),
            // The computer is busy, or the decoder can't run: making one wouldn't do better, and
            // would replace one that's fine.
            Err(Why::CantRun) => return Some(Outcome::Nothing),
            Err(_) => {}
        }
    }
    failed_before(cx, &job.path, mtime).then_some(Outcome::Nothing)
}

/// A file from the cache, if it's a file and no bigger than a thumbnail could be.
fn read_small(path: &Path) -> Option<Vec<u8>> {
    let mut file = File::options().read(true).custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW).open(path).ok()?;
    if !file.metadata().is_ok_and(|m| m.is_file() && m.len() <= thumbcache::MAX_THUMBNAIL) {
        return None;
    }
    let mut bytes = Vec::new();
    Read::by_ref(&mut file).take(thumbcache::MAX_THUMBNAIL).read_to_end(&mut bytes).ok()?;
    Some(bytes)
}

fn in_memory(bytes: &[u8]) -> Option<File> {
    let fd = memfd_create("gannet-thumbnail-in", MemfdFlags::CLOEXEC).ok()?;
    let mut file = File::from(fd);
    file.write_all(bytes).ok()?;
    seek(&file, SeekFrom::Start(0)).ok()?;
    Some(file)
}

fn failed_before(cx: &Cx, path: &Path, mtime: i64) -> bool {
    read_small(&thumbcache::failure(&cx.cache_home, path)).is_some_and(|note| thumbcache::recorded_mtime(&note) == Some(mtime) && thumbcache::text(&note, "Software") == Some(SOFTWARE.as_bytes()))
}

/// Why there's no picture.
#[derive(Debug, PartialEq)]
enum Why {
    /// Nothing here reads this kind of file.
    Unsupported,
    /// It was tried, and it didn't work. Trying again won't help until the file changes.
    Failed,
    /// Something needed isn't there, like bubblewrap or `ffmpeg`, or the sandbox couldn't start.
    /// That's this computer's fault, not the file's.
    CantRun,
    Stopped,
}

/// Decodes `input` in the sandboxed decoder, at most the "normal" size.
fn decode(input: File, cancel: &OwnedFd, limit: Duration) -> Result<ImageBuf, Why> {
    let mut helper = Helper::spawn_thumbnail(input).map_err(|_| Why::CantRun)?;
    match helper.wait(cancel, limit) {
        Waited::Exited(status) if status.success() => ImageBuf::from_memfd(&helper.output).ok_or(Why::Failed),
        Waited::Exited(status) => Err(match (status.code(), status.signal()) {
            (Some(decode::EXIT_UNSUPPORTED), _) => Why::Unsupported,
            (Some(decode::EXIT_NO_SANDBOX), _) | (None, Some(libc::SIGSYS)) => Why::CantRun,
            // Damaged, too big, or out of time or memory.
            _ => Why::Failed,
        }),
        Waited::Cancelled => Err(Why::Stopped),
        // These run at the lowest priority, so a busy computer can be why. Its CPU limit is what
        // stops a file that's really too much work.
        Waited::TimedOut => Err(Why::CantRun),
    }
}

/// Makes a thumbnail, and keeps it in the cache, or notes that it can't be made.
fn make(job: &Job, cx: &Cx, stop: &OwnedFd) -> Outcome {
    let Ok(meta) = std::fs::metadata(&job.path) else { return Outcome::Nothing };
    let mtime = meta.mtime();
    let made = match job.media {
        Media::Image => match preview::reopen(&job.path) {
            Some(file) => match decode(file, stop, MAKE_LIMIT) {
                Err(Why::Unsupported) => by_thumbnailer(&job.path, cx, stop),
                other => other,
            },
            None => Err(Why::CantRun),
        },
        Media::Video => by_ffmpeg(&job.path, stop),
    };
    // A thumbnail of a thumbnail would make another, forever.
    let keep = !thumbcache::is_inside(&cx.cache_home, &job.path);
    match made {
        Ok(image) => {
            if keep {
                let uri = thumbcache::thumbnail_uri(&job.path);
                if let Some(png) = encode(image.w, image.h, image.pixels(), &uri, mtime, "Gannet") {
                    let _ = write_new(&thumbcache::normal(&cx.cache_home, &job.path), &png);
                }
            }
            shown(&image, job.side)
        }
        Err(Why::Failed) => {
            if keep && let Some(note) = encode(1, 1, &[0; 4], &thumbcache::thumbnail_uri(&job.path), mtime, SOFTWARE) {
                let _ = write_new(&thumbcache::failure(&cx.cache_home, &job.path), &note);
            }
            Outcome::Nothing
        }
        Err(Why::Unsupported | Why::CantRun) => Outcome::Nothing,
        Err(Why::Stopped) => Outcome::Stopped,
    }
}

/// What happened to a jailed program, as far as a thumbnail goes.
fn ran(result: Result<(std::process::ExitStatus, OwnedFd), NotRun>, stop: &OwnedFd) -> Result<OwnedFd, Why> {
    match result {
        Ok((status, output)) if status.success() => Ok(output),
        Ok(_) => Err(Why::Failed),
        Err(NotRun::NoBubblewrap) => Err(Why::CantRun),
        Err(NotRun::Stopped) if preview::cancelled_within(stop, Duration::ZERO) => Err(Why::Stopped),
        // It couldn't start, or ran out of time on a busy computer: neither is the file's fault.
        Err(NotRun::Stopped) => Err(Why::CantRun),
    }
}

/// Decodes the PNG a program wrote, which it might have got wrong.
fn decode_output(output: OwnedFd, stop: &OwnedFd) -> Result<ImageBuf, Why> {
    seek(&output, SeekFrom::Start(0)).map_err(|_| Why::Failed)?;
    decode(File::from(output), stop, MAKE_LIMIT).map_err(|why| if why == Why::Unsupported { Why::Failed } else { why })
}

/// A picture by the system's thumbnailer for the file's type, for pictures the decoder can't
/// read, like SVG and HEIC.
fn by_thumbnailer(path: &Path, cx: &Cx, stop: &OwnedFd) -> Result<ImageBuf, Why> {
    let mut head = vec![0u8; mime::HEAD];
    let file = preview::reopen(path).ok_or(Why::CantRun)?;
    let n = std::os::unix::fs::FileExt::read_at(&file, &mut head, 0).unwrap_or(0);
    let name = path.file_name().map_or(&b""[..], |n| n.as_encoded_bytes());
    let mime = cx.kinds.db().guess(name, Some(&head[..n]));
    let ext = tools::extension(path);
    let mut tried = Err(Why::Unsupported);
    for thumbnailer in cx.kinds.thumbnailers().find_all(mime) {
        let Some(program) = thumbnailer.program(&[PathBuf::from(tools::PROGRAMS)]) else { continue };
        let Some(argv) = tools::thumbnailer(|input, output| thumbnailer.command(&program, input, output, thumbcache::NORMAL_PIXELS), &ext) else { continue };
        let file = preview::reopen(path).ok_or(Why::CantRun)?;
        tried = ran(preview::run_tool(&argv, file, Some(&ext), stop), stop).and_then(|out| decode_output(out, stop));
        if matches!(tried, Ok(_) | Err(Why::Stopped)) {
            break;
        }
    }
    tried
}

/// A frame of a video, by `ffmpeg` in the jail.
fn by_ffmpeg(path: &Path, stop: &OwnedFd) -> Result<ImageBuf, Why> {
    if !Path::new(tools::PROGRAMS).join("ffmpeg").is_file() {
        return Err(Why::CantRun);
    }
    let ext = tools::extension(path);
    let file = preview::reopen(path).ok_or(Why::CantRun)?;
    let output = ran(preview::run_tool(&tools::video_frame(&ext, thumbcache::NORMAL_PIXELS), file, Some(&ext), stop), stop)?;
    decode_output(output, stop)
}

/// A PNG of these pixels that says what it's a thumbnail of, as the freedesktop spec wants.
fn encode(w: u32, h: u32, rgba: &[u8], uri: &str, mtime: i64, software: &str) -> Option<Vec<u8>> {
    use image::ImageEncoder;
    let mut plain = Vec::new();
    image::codecs::png::PngEncoder::new_with_quality(&mut plain, image::codecs::png::CompressionType::Fast, image::codecs::png::FilterType::Adaptive).write_image(rgba, w, h, image::ExtendedColorType::Rgba8).ok()?;
    // The encoder doesn't write text, so it goes in after the header, which always comes first.
    const IHDR_END: usize = 8 + 4 + 4 + 13 + 4;
    if plain.get(12..16) != Some(b"IHDR") || plain.len() < IHDR_END {
        return None;
    }
    let mut out = plain[..IHDR_END].to_vec();
    for (key, value) in [("Thumb::URI", uri.to_string()), ("Thumb::MTime", mtime.to_string()), ("Software", software.to_string())] {
        let body = [key.as_bytes(), b"\0", value.as_bytes()].concat();
        out.extend_from_slice(&(body.len() as u32).to_be_bytes());
        let typed = [&b"tEXt"[..], &body].concat();
        out.extend_from_slice(&typed);
        out.extend_from_slice(&crc32(&typed).to_be_bytes());
    }
    out.extend_from_slice(&plain[IHDR_END..]);
    Some(out)
}

/// The checksum each PNG chunk ends with.
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in bytes {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { 0xedb8_8320 ^ (crc >> 1) } else { crc >> 1 };
        }
    }
    !crc
}

/// Writes a file whole or not at all, as the spec asks: other programs read the cache at any
/// moment. Only its owner can read it, or the folders it's in.
fn write_new(target: &Path, bytes: &[u8]) -> std::io::Result<()> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let dir = target.parent().ok_or(std::io::ErrorKind::InvalidInput)?;
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    let tmp = dir.join(format!(".gannet-{}-{}.png", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    let written = File::options().write(true).create_new(true).mode(0o600).open(&tmp).and_then(|mut f| f.write_all(bytes)).and_then(|()| std::fs::rename(&tmp, target));
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

/// The row's picture of a thumbnail, or its icon if the picture would be too small to make out.
fn shown(image: &ImageBuf, side: u32) -> Outcome {
    match fit_pixels(image.w, image.h, image.pixels(), side) {
        Some(pic) => Outcome::Shown(Arc::new(pic)),
        None => Outcome::Nothing,
    }
}

/// The middle square of a thumbnail, shrunk to `side` if it's bigger. `None` if it's less than
/// half that, like a thin banner's middle or a tiny picture, which would be a dot in the icon's
/// place.
fn fit_pixels(w: u32, h: u32, rgba: &[u8], side: u32) -> Option<Pic> {
    let (w, h) = (w as usize, h as usize);
    let s = w.min(h);
    if s * 2 < side as usize {
        return None;
    }
    let (x0, y0) = ((w - s) / 2, (h - s) / 2);
    let mut square = Vec::with_capacity(s * s * 4);
    for y in y0..y0 + s {
        square.extend_from_slice(&rgba[(y * w + x0) * 4..(y * w + x0 + s) * 4]);
    }
    let side = (side as usize).max(1);
    if s <= side {
        return Some(Pic { w: s as u32, h: s as u32, rgba: square });
    }
    let rgba = preview::box_scale(&square, s, s, side, side, &AtomicBool::new(false))?;
    Some(Pic { w: side as u32, h: side as u32, rgba })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A folder of its own for one test, gone afterwards.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("gannet-test-thumbs-{}-{tag}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn hub() -> (Hub, std::sync::mpsc::Receiver<Msg>) {
        let (tx, rx) = std::sync::mpsc::channel();
        (Hub { tx, wake: Arc::new(eventfd(0, EventfdFlags::CLOEXEC).unwrap()) }, rx)
    }

    fn thumbs() -> Thumbs {
        Thumbs::new(hub().0, PathBuf::from("/nowhere"), Arc::new(Kinds::default()))
    }

    fn row(name: &str, mtime: Option<i64>) -> (PathBuf, Media, Option<i64>) {
        (PathBuf::from(format!("/f/{name}")), Media::Image, mtime)
    }

    fn queued(t: &Thumbs) -> (Vec<String>, Vec<String>) {
        let q = t.shared.queue.lock().unwrap();
        let names = |lane: &VecDeque<Job>| lane.iter().map(|j| j.path.file_name().unwrap().to_string_lossy().into_owned()).collect();
        (names(&q.reading), names(&q.making))
    }

    fn done(t: &mut Thumbs, name: &str, outcome: Outcome) -> bool {
        t.on_done(Done { path: PathBuf::from(format!("/f/{name}")), side: t.side, outcome })
    }

    fn pic() -> Outcome {
        Outcome::Shown(Arc::new(Pic { w: 1, h: 1, rgba: vec![0; 4] }))
    }

    #[test]
    fn only_pictures_and_videos_that_are_files() {
        assert_eq!(media(Kind::File, b"a.JPG"), Some(Media::Image));
        assert_eq!(media(Kind::Link, b"a.mkv"), Some(Media::Video));
        assert_eq!(media(Kind::File, b"a.txt"), None);
        assert_eq!(media(Kind::Dir, b"photos.png"), None);
        assert_eq!(media(Kind::LinkDir, b"x.mp4"), None);
    }

    #[test]
    fn rows_are_asked_for_once_top_first() {
        let mut t = thumbs();
        assert!(t.plan(&[row("a", Some(1)), row("b", None)], 20));
        assert_eq!(queued(&t).0, ["a", "b"]);
        assert!(!t.plan(&[row("a", Some(1)), row("b", None)], 20), "nothing new");
        assert_eq!(queued(&t).0, ["a", "b"]);
        assert_eq!(t.waiting(), 2);
    }

    #[test]
    fn rows_scrolled_past_are_dropped_and_asked_again_when_back() {
        let mut t = thumbs();
        t.plan(&[row("a", None), row("b", None)], 20);
        t.shared.queue.lock().unwrap().making.push_back(Job { path: "/f/c".into(), media: Media::Video, side: 20 });
        t.plan(&[row("b", None), row("d", None)], 20);
        assert_eq!(queued(&t), (vec!["b".to_string(), "d".to_string()], vec![]), "a and c are gone from both queues");
        assert_eq!(t.waiting(), 2, "and a is no longer waited on");
        t.plan(&[row("a", None)], 20);
        assert_eq!(queued(&t).0, ["a"], "back on screen, it's asked for again");
        // What the cache reader checks before passing one on to be made.
        assert!(t.shared.queue.lock().unwrap().on_screen.contains(Path::new("/f/a")));
        t.plan(&[], 20);
        assert!(t.shared.queue.lock().unwrap().on_screen.is_empty() && queued(&t).0.is_empty(), "a folder with none");
    }

    #[test]
    fn one_being_made_is_stopped_when_scrolled_past() {
        let mut t = thumbs();
        let stop = Arc::new(eventfd(0, EventfdFlags::CLOEXEC | EventfdFlags::NONBLOCK).unwrap());
        t.plan(&[row("a", None)], 20);
        t.shared.queue.lock().unwrap().reading.clear();
        t.shared.queue.lock().unwrap().running.push(("/f/a".into(), stop.clone()));
        t.plan(&[row("a", None)], 20);
        assert!(!preview::cancelled_within(&stop, Duration::ZERO), "still on screen");
        t.plan(&[row("b", None)], 20);
        assert!(preview::cancelled_within(&stop, Duration::ZERO));
        // It comes back stopped, and is asked for again when it's back.
        assert!(!done(&mut t, "a", Outcome::Stopped));
        t.plan(&[row("a", None)], 20);
        assert_eq!(queued(&t).0, ["a"], "a again, and b is gone");
    }

    #[test]
    fn answers_are_kept_and_a_changed_file_is_asked_again() {
        let mut t = thumbs();
        t.plan(&[row("a", Some(5)), row("b", None), row("c", None)], 20);
        t.shared.queue.lock().unwrap().reading.clear();
        assert!(done(&mut t, "a", pic()));
        assert!(done(&mut t, "b", Outcome::Nothing));
        assert!(t.picture(Path::new("/f/a")).is_some() && t.picture(Path::new("/f/b")).is_none());
        assert!(!t.plan(&[row("a", Some(5)), row("b", Some(9))], 20), "b's time is only now known: what came back is for it");
        assert!(t.plan(&[row("a", Some(6)), row("b", Some(9))], 20));
        assert_eq!(queued(&t).0, ["a"], "a has changed");
        assert!(t.picture(Path::new("/f/a")).is_none(), "and its old picture isn't shown meanwhile");
        assert!(!t.plan(&[row("a", Some(6))], 20));
        assert!(!done(&mut t, "zzz", pic()), "nothing asked for it");
    }

    #[test]
    fn another_size_asks_again_and_late_answers_are_ignored() {
        let mut t = thumbs();
        t.plan(&[row("a", None)], 20);
        t.shared.queue.lock().unwrap().reading.clear();
        assert!(t.plan(&[row("a", None)], 40), "at another scale");
        assert!(!t.on_done(Done { path: "/f/a".into(), side: 20, outcome: pic() }), "the old size's answer");
        assert!(t.picture(Path::new("/f/a")).is_none());
        assert_eq!(queued(&t).0, ["a"]);
    }

    #[test]
    fn the_oldest_are_forgotten_but_never_those_waited_on() {
        let mut t = thumbs();
        let rows: Vec<_> = (0..KEEP + 50).map(|i| row(&i.to_string(), None)).collect();
        for r in &rows[..KEEP] {
            t.plan(std::slice::from_ref(r), 20);
            t.on_done(Done { path: r.0.clone(), side: 20, outcome: pic() });
        }
        t.plan(&rows[KEEP..], 20);
        assert_eq!(t.known.len(), KEEP);
        assert!(t.picture(Path::new("/f/0")).is_none(), "the oldest went");
        assert!(t.picture(&rows[KEEP - 1].0).is_some());
        assert_eq!(t.waiting(), 50, "all those asked for are still waited on");
        assert_eq!(t.order.len(), t.known.len());
    }

    #[test]
    fn a_row_gets_the_middle_square_shrunk_to_fit() {
        // 4x2: left half red, right half blue, so the middle square is half of each.
        let mut px = Vec::new();
        for _ in 0..2 {
            px.extend_from_slice(&[[255, 0, 0, 255], [255, 0, 0, 255], [0, 0, 255, 255], [0, 0, 255, 255]].concat());
        }
        let p = fit_pixels(4, 2, &px, 4).unwrap();
        assert_eq!((p.w, p.h), (2, 2), "small ones aren't made bigger");
        assert_eq!(&p.rgba[..8], [255, 0, 0, 255, 0, 0, 255, 255]);
        let big = vec![200u8; 100 * 60 * 4];
        let p = fit_pixels(100, 60, &big, 20).unwrap();
        assert_eq!((p.w, p.h, p.rgba.len()), (20, 20, 20 * 20 * 4));
        assert_eq!(p.rgba[0], 200);
        let p = fit_pixels(10, 300, &vec![9u8; 10 * 300 * 4], 20).unwrap();
        assert_eq!((p.w, p.h), (10, 10), "a thin strip is its middle, not stretched");
        // Less than half the size would be a dot where the icon was.
        assert!(fit_pixels(1000, 3, &vec![9u8; 1000 * 3 * 4], 20).is_none(), "a banner");
        assert!(fit_pixels(1, 1, &[9; 4], 20).is_none(), "a single pixel");
    }

    #[test]
    fn a_row_scrolled_away_and_back_is_in_the_order_once() {
        let mut t = thumbs();
        // Stopped while being made, and asked for again, several times.
        for _ in 0..5 {
            t.plan(&[row("a", None)], 20);
            t.shared.queue.lock().unwrap().reading.clear();
            assert!(!done(&mut t, "a", Outcome::Stopped));
        }
        t.plan(&[row("a", None)], 20);
        assert_eq!(t.order.iter().filter(|p| p.ends_with("a")).count(), 1, "{:?}", t.order);
    }

    #[test]
    fn chunk_checksums() {
        // Every PNG ends with this chunk, and so with this checksum.
        assert_eq!(crc32(b"IEND"), 0xae42_6082);
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
    }

    #[test]
    fn thumbnails_written_say_what_they_are_of() {
        let s = Scratch::new("written");
        let target = s.path().join("cache/thumbnails/normal/x.png");
        let png = encode(2, 1, &[1, 2, 3, 255, 4, 5, 6, 128], "file:///a%20b.png", 1234, "Gannet").unwrap();
        write_new(&target, &png).unwrap();
        let bytes = std::fs::read(&target).unwrap();
        assert_eq!(thumbcache::recorded_mtime(&bytes), Some(1234));
        assert_eq!(thumbcache::text(&bytes, "Thumb::URI"), Some(&b"file:///a%20b.png"[..]));
        let decoded = image::load_from_memory(&bytes).unwrap().into_rgba8();
        assert_eq!(decoded.as_raw(), &[1, 2, 3, 255, 4, 5, 6, 128]);
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&target).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::metadata(target.parent().unwrap()).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(std::fs::read_dir(target.parent().unwrap()).unwrap().count(), 1, "nothing left over");
        // Replacing one is whole too.
        write_new(&target, b"second").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"second");
    }

    #[test]
    fn failures_are_believed_only_for_the_same_file_and_version() {
        let s = Scratch::new("failures");
        let cx = Cx { cache_home: s.path().to_path_buf(), kinds: Arc::new(Kinds::default()) };
        let file = s.path().join("broken.png");
        let note = |mtime, software| {
            let png = encode(1, 1, &[0; 4], &thumbcache::thumbnail_uri(&file), mtime, software).unwrap();
            write_new(&thumbcache::failure(s.path(), &file), &png).unwrap();
        };
        assert!(!failed_before(&cx, &file, 10));
        note(10, SOFTWARE);
        assert!(failed_before(&cx, &file, 10));
        assert!(!failed_before(&cx, &file, 11), "it's changed since");
        note(10, "Gannet 0.0.1");
        assert!(!failed_before(&cx, &file, 10), "an older Gannet's note");
    }
}
