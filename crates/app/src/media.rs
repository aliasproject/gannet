//! Video previews. libmpv isn't memory-safe and plays whatever files people have, so it runs in a
//! copy of this program inside a bubblewrap jail: no home, no network, no other processes, read-only
//! system files, the one video on a descriptor, and a sound socket if there's one to use. A seccomp
//! filter goes on top. Being a jail rather than the image decoder's allowlist, it has to let libmpv
//! run threads and read its own libraries, so it names what's forbidden.
//!
//! What this doesn't stop: code that took over the player can read the one video and system files,
//! draw into its frames, and use the sound socket. A sound server normally lets its clients record
//! too, so with sound on that can include the microphone.
//!
//! Frames arrive in shared memory, and everything else as lines of text on two pipes. Everything
//! from the player is checked before it's believed.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::fs::{MemfdFlags, SealFlags, fcntl_add_seals, memfd_create};
use rustix::mm::{MapFlags, ProtFlags, mmap, munmap};
use rustix::pipe::{PipeFlags, pipe_with};

use crate::decode;

/// The argument that makes this program a player rather than a window.
pub const ARG: &str = "--video-helper";
pub const EXIT_NO_MPV: i32 = 7;
pub const EXIT_CANT_PLAY: i32 = 8;

/// The largest frame the player draws. A bigger popup scales it up.
pub const MAX_W: u32 = 3840;
pub const MAX_H: u32 = 2160;
const FRAME_BYTES: usize = MAX_W as usize * MAX_H as usize * 4;
/// Two frames, one being drawn and one showing. Memory only gets used once it's touched.
const SHARED_BYTES: usize = FRAME_BYTES * 2;

const FD_VIDEO: c_int = 3;
const FD_FRAMES: c_int = 4;
/// Where the program itself and the sound socket appear inside the jail.
const JAILED_SELF: &str = "/gannet-helper";
const JAILED_SOUND: &str = "/sound-server";
/// Longer than any line the player has a reason to say.
const MAX_LINE: usize = 128;
const EXIT_GRACE: Duration = Duration::from_millis(500);
/// A little under one frame at 120 fps.
const DELIVER_GAP: Duration = Duration::from_millis(7);
const REPORT_EVERY: Duration = Duration::from_millis(100);
const RESEND_EVERY: Duration = Duration::from_secs(1);
const MAX_MEMORY: u64 = 16 * 1024 * 1024 * 1024;

// ---- What the two sides say ----

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Said {
    /// It got as far as running inside the jail, so a failure after this isn't the jail's.
    Hello,
    Frame {
        index: usize,
        w: u32,
        h: u32,
    },
    State(PlayState),
    NoVideo,
    CantPlay,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PlayState {
    pub position: f64,
    pub duration: f64,
    pub paused: bool,
    pub volume: f64,
    pub muted: bool,
}

/// A line from the player, or `None` if it isn't a sensible one.
pub fn parse_said(line: &str) -> Option<Said> {
    let mut words = line.split_ascii_whitespace();
    let said = match words.next()? {
        "hello" => Said::Hello,
        "novideo" => Said::NoVideo,
        "cantplay" => Said::CantPlay,
        "frame" => {
            let index: usize = words.next()?.parse().ok()?;
            let w: u32 = words.next()?.parse().ok()?;
            let h: u32 = words.next()?.parse().ok()?;
            if index > 1 || w == 0 || h == 0 || w > MAX_W || h > MAX_H {
                return None;
            }
            Said::Frame { index, w, h }
        }
        "state" => {
            let number = |word: Option<&str>| {
                word?
                    .parse::<f64>()
                    .ok()
                    .filter(|n| n.is_finite() && *n >= 0.0)
            };
            let flag = |word: Option<&str>| word.map(|w| w == "1");
            Said::State(PlayState {
                position: number(words.next())?,
                duration: number(words.next())?,
                paused: flag(words.next())?,
                volume: number(words.next())?.min(100.0),
                muted: flag(words.next())?,
            })
        }
        _ => return None,
    };
    words.next().is_none().then_some(said)
}

// ---- The app's side ----

/// The frames another process draws. Dropping this hands the memory back.
pub struct Frames {
    ptr: *mut c_void,
}

// SAFETY: mapped read-only here and sealed against shrinking, and only `Drop` unmaps it. The player
// can write these pages at any time, so what's read can change mid-frame, and any bytes are valid
// pixels. The worst it can do is a torn or nonsense picture.
unsafe impl Send for Frames {}
// SAFETY: as above.
unsafe impl Sync for Frames {}

impl Frames {
    fn map(fd: &OwnedFd) -> Option<Self> {
        // SAFETY: a new read-only mapping at an address the kernel chooses, of a file that's
        // already that size and can't shrink.
        let ptr = unsafe {
            mmap(
                std::ptr::null_mut(),
                SHARED_BYTES,
                ProtFlags::READ,
                MapFlags::SHARED,
                fd,
                0,
            )
        }
        .ok()?;
        Some(Self { ptr })
    }

    /// One frame, in the canvas's own pixel format. `parse_said` has checked the sizes.
    pub fn frame(&self, index: usize, w: u32, h: u32) -> &[u32] {
        debug_assert!(index <= 1 && w <= MAX_W && h <= MAX_H);
        // SAFETY: two frames of `FRAME_BYTES` are mapped until drop, page-aligned, and any bytes
        // are valid `u32`s.
        unsafe {
            std::slice::from_raw_parts(
                (self.ptr as *const u8).add(index.min(1) * FRAME_BYTES) as *const u32,
                w as usize * h as usize,
            )
        }
    }
}

impl Drop for Frames {
    fn drop(&mut self) {
        // SAFETY: this is the mapping from `map`, and nothing can still borrow it.
        let _ = unsafe { munmap(self.ptr, SHARED_BYTES) };
    }
}

/// Telling a running player what to do.
pub struct Remote {
    commands: File,
}

impl Remote {
    /// Never waits: a player too busy to read just misses this one.
    fn say(&mut self, line: &str) {
        let _ = self.commands.write_all(format!("{line}\n").as_bytes());
    }
    pub fn set_paused(&mut self, paused: bool) {
        self.say(if paused { "pause" } else { "play" });
    }
    pub fn seek_by(&mut self, seconds: f64) {
        self.say(&format!("seek {seconds:.1}"));
    }
    pub fn seek_to(&mut self, fraction: f64) {
        self.say(&format!("seekto {:.2}", fraction.clamp(0.0, 1.0) * 100.0));
    }
    pub fn set_volume(&mut self, volume: f64) {
        self.say(&format!("volume {:.0}", volume.clamp(0.0, 100.0)));
    }
    pub fn set_muted(&mut self, muted: bool) {
        self.say(if muted { "mute" } else { "unmute" });
    }
    pub fn set_size(&mut self, w: i32, h: i32) {
        self.say(&format!(
            "size {} {}",
            w.clamp(1, MAX_W as i32),
            h.clamp(1, MAX_H as i32)
        ));
    }
}

/// A player's handles with nothing on the other end, for tests of what the app does with them.
#[cfg(test)]
pub fn parts_for_test() -> (Remote, Frames) {
    let shared = memfd_create("test", MemfdFlags::CLOEXEC).unwrap();
    rustix::fs::ftruncate(&shared, SHARED_BYTES as u64).unwrap();
    let (_read, write) = pipe_with(PipeFlags::CLOEXEC | PipeFlags::NONBLOCK).unwrap();
    (
        Remote {
            commands: File::from(write),
        },
        Frames::map(&shared).unwrap(),
    )
}

/// The jail every player gets, whatever the video: each bubblewrap option with its values.
const JAIL_RULES: &[(&str, &[&str])] = &[
    // The process: alone, without privileges, and gone when the app is.
    ("--unshare-all", &[]),
    ("--cap-drop", &["ALL"]),
    ("--new-session", &[]),
    ("--die-with-parent", &[]),
    // System files, read-only. No /proc: /proc/self/mem would write to code pages without
    // asking for executable memory.
    ("--ro-bind", &["/usr", "/usr"]),
    ("--symlink", &["usr/bin", "/bin"]),
    ("--symlink", &["usr/lib", "/lib"]),
    ("--symlink", &["usr/lib", "/lib64"]),
    // Finding libraries without this works, but searches every folder for each one.
    ("--ro-bind-try", &["/etc/ld.so.cache", "/etc/ld.so.cache"]),
    ("--dev", &["/dev"]),
    ("--tmpfs", &["/tmp"]),
    // An environment with nothing of the user's in it.
    ("--clearenv", &[]),
    ("--setenv", &["HOME", "/tmp"]),
    ("--setenv", &["LC_ALL", "C"]),
];

/// The command line for bubblewrap: what the player can see, and nothing else.
pub fn jail_arguments(executable: &Path, sound_socket: Option<&Path>) -> Vec<String> {
    let mut args = Vec::new();
    let mut rule = |option: &str, values: &[&str]| {
        args.push(option.to_owned());
        args.extend(values.iter().map(|value| (*value).to_owned()));
    };
    for (option, values) in JAIL_RULES {
        rule(option, values);
    }
    rule("--ro-bind", &[&executable.to_string_lossy(), JAILED_SELF]);
    if let Some(socket) = sound_socket {
        rule("--ro-bind", &[&socket.to_string_lossy(), JAILED_SOUND]);
        rule(
            "--setenv",
            &["PULSE_SERVER", &format!("unix:{JAILED_SOUND}")],
        );
    }
    rule("--", &[JAILED_SELF]);
    args
}

/// The session's sound server, if it has one where PulseAudio clients look.
pub fn sound_socket() -> Option<std::path::PathBuf> {
    let runtime_dir = std::path::PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR")?);
    Some(runtime_dir.join("pulse/native")).filter(|path| path.exists())
}

pub enum StartError {
    NoBubblewrap,
    Other,
}

/// A player that's been started, for the thread that looks after it.
pub struct Started {
    pub child: Child,
    pub said: File,
    pub remote: Remote,
    pub frames: Frames,
}

/// Starts `words` after the jail's own, with the video and frame memory on their descriptors.
pub fn start_jailed(video: File, size: (i32, i32), words: &[&str]) -> Result<Started, StartError> {
    let shared = memfd_create(
        "gannet-video",
        MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING,
    )
    .map_err(|_| StartError::Other)?;
    rustix::fs::ftruncate(&shared, SHARED_BYTES as u64).map_err(|_| StartError::Other)?;
    // Without this, a player shrinking it would fault the app when it read a frame.
    fcntl_add_seals(&shared, SealFlags::SHRINK | SealFlags::GROW).map_err(|_| StartError::Other)?;
    let frames = Frames::map(&shared).ok_or(StartError::Other)?;
    let executable = std::env::current_exe().map_err(|_| StartError::Other)?;
    let (their_commands, our_commands) =
        pipe_with(PipeFlags::CLOEXEC).map_err(|_| StartError::Other)?;
    let (our_said, their_said) = pipe_with(PipeFlags::CLOEXEC).map_err(|_| StartError::Other)?;
    rustix::io::ioctl_fionbio(&our_commands, true).map_err(|_| StartError::Other)?;
    // Well away from the numbers they're about to be given.
    let high = |fd: c_int| {
        // SAFETY: duplicating a descriptor we hold, and only taking ownership of a good result.
        let copy = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 64) };
        (copy >= 0)
            .then(|| unsafe { OwnedFd::from_raw_fd(copy) })
            .ok_or(StartError::Other)
    };
    let (video_copy, shared_copy) = (high(video.as_raw_fd())?, high(shared.as_raw_fd())?);
    let (video_fd, shared_fd) = (video_copy.as_raw_fd(), shared_copy.as_raw_fd());
    let mut command = Command::new("bwrap");
    command
        .args(jail_arguments(&executable, sound_socket().as_deref()))
        .args(words);
    command.arg(size.0.to_string()).arg(size.1.to_string());
    command
        .stdin(Stdio::from(their_commands))
        .stdout(Stdio::from(their_said))
        .stderr(Stdio::null());
    // SAFETY: only async-signal-safe calls. Bubblewrap passes on all it inherits.
    unsafe {
        command.pre_exec(move || {
            if libc::dup2(video_fd, FD_VIDEO) < 0 || libc::dup2(shared_fd, FD_FRAMES) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            // Refusing beats handing the jail whatever else this process has open.
            if libc::close_range(
                (FD_FRAMES + 1) as libc::c_uint,
                libc::c_uint::MAX,
                libc::CLOSE_RANGE_CLOEXEC as libc::c_int,
            ) != 0
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            StartError::NoBubblewrap
        } else {
            StartError::Other
        }
    })?;
    drop(command);
    Ok(Started {
        child,
        said: File::from(our_said),
        remote: Remote {
            commands: File::from(our_commands),
        },
        frames,
    })
}

/// What the player has said since it was last asked. Only the latest frame and state matter, so a
/// player talking faster than anyone's listening costs nothing.
#[derive(Default)]
struct Inbox {
    hello: bool,
    no_video: bool,
    cant_play: bool,
    frame: Option<Said>,
    state: Option<Said>,
}

impl Inbox {
    fn hear(&mut self, said: Said) {
        match said {
            Said::Hello => self.hello = true,
            Said::NoVideo => self.no_video = true,
            Said::CantPlay => self.cant_play = true,
            Said::Frame { .. } => self.frame = Some(said),
            Said::State(_) => self.state = Some(said),
        }
    }

    fn deliver(&mut self, on_said: &mut impl FnMut(Said)) -> bool {
        let inbox = std::mem::take(self);
        let singles = [
            (inbox.hello, Said::Hello),
            (inbox.no_video, Said::NoVideo),
            (inbox.cant_play, Said::CantPlay),
        ];
        let mut any = false;
        for said in singles
            .into_iter()
            .filter(|s| s.0)
            .map(|s| s.1)
            .chain(inbox.state)
            .chain(inbox.frame)
        {
            on_said(said);
            any = true;
        }
        any
    }
}

/// Reads lines, sorting them into the inbox. Lines come out whole or not at all: one that's too long
/// or not text is skipped, and goes no further than this buffer.
fn read_lines(said: File, inbox: &Mutex<Inbox>, ready: &OwnedFd) {
    let mut reader = BufReader::with_capacity(4096, said);
    let mut line = Vec::with_capacity(MAX_LINE);
    let mut too_long = false;
    loop {
        let available = match reader.fill_buf() {
            Ok(available) => available,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return,
        };
        if available.is_empty() {
            return;
        }
        let (part, had_newline) = match available.iter().position(|&b| b == b'\n') {
            Some(at) => (&available[..at], true),
            None => (available, false),
        };
        too_long |= line.len() + part.len() > MAX_LINE;
        if !too_long {
            line.extend_from_slice(part);
        }
        let used = part.len() + usize::from(had_newline);
        if had_newline {
            if !too_long && let Some(said) = std::str::from_utf8(&line).ok().and_then(parse_said) {
                inbox.lock().unwrap_or_else(|e| e.into_inner()).hear(said);
                // A full pipe already says there's something to read.
                let _ = rustix::io::write(ready, &[1u8]);
            }
            line.clear();
            too_long = false;
        }
        reader.consume(used);
    }
}

/// Watches a running player until `cancel` fires or it stops, passing on what it says. Returns its
/// exit status, or `None` if cancelled. Either way the player is gone afterwards, whatever it does
/// with its pipes, and how much it says makes no difference to how often `on_said` runs.
pub fn watch(
    mut child: Child,
    said: File,
    cancel: &OwnedFd,
    mut on_said: impl FnMut(Said),
) -> Option<ExitStatus> {
    let stop = |child: &mut Child| {
        let _ = child.kill();
        let _ = child.wait();
    };
    let Ok((lines_ready, line_written)) = pipe_with(PipeFlags::CLOEXEC | PipeFlags::NONBLOCK)
    else {
        stop(&mut child);
        return None;
    };
    let inbox = Arc::new(Mutex::new(Inbox::default()));
    let reading = inbox.clone();
    // It ends when the pipe does, and it isn't waited for: the player might have passed its end on.
    let reader = std::thread::Builder::new()
        .name("gannet-video-read".into())
        .spawn(move || read_lines(said, &reading, &line_written));
    if reader.is_err() {
        stop(&mut child);
        return None;
    }
    let mut buf = [0u8; 4096];
    let mut pipe_ended_at: Option<Instant> = None;
    loop {
        let wait = if pipe_ended_at.is_some() {
            Duration::from_millis(5)
        } else {
            Duration::from_secs(1)
        };
        let timeout = Timespec {
            tv_sec: wait.as_secs() as _,
            tv_nsec: wait.subsec_nanos() as _,
        };
        let mut fds = [
            PollFd::new(cancel, PollFlags::IN),
            PollFd::new(&lines_ready, PollFlags::IN),
        ];
        let watching = if pipe_ended_at.is_some() { 1 } else { 2 };
        let _ = poll(&mut fds[..watching], Some(&timeout));
        if !fds[0].revents().is_empty() {
            stop(&mut child);
            return None;
        }
        if pipe_ended_at.is_none()
            && !fds[1].revents().is_empty()
            && matches!(rustix::io::read(&lines_ready, &mut buf), Ok(0))
        {
            pipe_ended_at = Some(Instant::now());
        }
        let any = inbox
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .deliver(&mut on_said);
        if let Some(ended) = pipe_ended_at {
            match child.try_wait() {
                Ok(Some(status)) => return Some(status),
                // A player with no way to report anything has nothing left to do.
                Ok(None) if ended.elapsed() >= EXIT_GRACE => stop(&mut child),
                Ok(None) => {}
                Err(_) => return None,
            }
        } else if any {
            // However fast the player talks, the app hears about it at this rate at most.
            let pause = Timespec {
                tv_sec: 0,
                tv_nsec: DELIVER_GAP.subsec_nanos() as _,
            };
            let mut cancel_only = [PollFd::new(cancel, PollFlags::IN)];
            let _ = poll(&mut cancel_only, Some(&pause));
        }
    }
}

/// How a player ended, read through bubblewrap, which reports a child's signal as 128 and the number.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ending {
    NoMpv,
    NoSandbox,
    /// mpv is there, but it wouldn't start.
    PlayerFailed,
    SandboxStopped,
    JailFailed,
    Other,
}

pub fn ending(status: ExitStatus, said_hello: bool) -> Ending {
    let by_sandbox =
        status.signal() == Some(libc::SIGSYS) || status.code() == Some(128 + libc::SIGSYS);
    match status.code() {
        _ if by_sandbox => Ending::SandboxStopped,
        Some(EXIT_NO_MPV) => Ending::NoMpv,
        Some(decode::EXIT_NO_SANDBOX) => Ending::NoSandbox,
        Some(EXIT_CANT_PLAY) => Ending::PlayerFailed,
        // Bubblewrap's own failures, before the player was ever running.
        Some(1) if !said_hello => Ending::JailFailed,
        // Something crashed loading libmpv.
        _ if !said_hello => Ending::PlayerFailed,
        _ => Ending::Other,
    }
}

/// Whether a file is worth trying as a video, from how it starts. Containers that hold only sound
/// get told apart once the player has looked. Ogg is left out, as it's nearly always just sound.
pub fn looks_like_video(head: &[u8]) -> bool {
    let at = |offset: usize, magic: &[u8]| head.get(offset..offset + magic.len()) == Some(magic);
    // The same container holds photos.
    const STILL_BRANDS: [&[u8]; 12] = [
        b"heic", b"heix", b"heim", b"heis", b"hevc", b"hevm", b"hevs", b"avif", b"mif1", b"mif2",
        b"msf1", b"crx ",
    ];
    if at(4, b"ftyp") {
        return !STILL_BRANDS.iter().any(|brand| at(8, brand));
    }
    // A packet every 188 bytes, three in a row.
    let packets = |first: usize| (0..3).all(|i| at(first + i * 188, &[0x47]));
    // These words start plenty of text, so the size before them has to look like one. Text never
    // starts with a zero byte, and an atom this early isn't 16 MB.
    let padding = at(0, &[0])
        && [&b"wide"[..], b"free", b"skip"]
            .iter()
            .any(|atom| at(4, atom));
    padding
        || at(4, b"moov")
        || at(4, b"mdat")
        || at(0, &[0x1a, 0x45, 0xdf, 0xa3])
        || (at(0, b"RIFF") && at(8, b"AVI "))
        || at(0, b"FLV\x01")
        || at(0, &[0, 0, 1, 0xba])
        || at(0, &[0x30, 0x26, 0xb2, 0x75])
        || packets(0)
        || packets(4)
}

// ---- The player's side ----

type Handle = *mut c_void;

struct Mpv {
    create: unsafe extern "C" fn() -> Handle,
    set_option: unsafe extern "C" fn(Handle, *const c_char, *const c_char) -> c_int,
    initialize: unsafe extern "C" fn(Handle) -> c_int,
    command_string: unsafe extern "C" fn(Handle, *const c_char) -> c_int,
    get_string: unsafe extern "C" fn(Handle, *const c_char) -> *mut c_char,
    free: unsafe extern "C" fn(*mut c_void),
    wait_event: unsafe extern "C" fn(Handle, f64) -> *const RawEvent,
    set_wakeup:
        unsafe extern "C" fn(Handle, Option<unsafe extern "C" fn(*mut c_void)>, *mut c_void),
    observe: unsafe extern "C" fn(Handle, u64, *const c_char, c_int) -> c_int,
    render_create: unsafe extern "C" fn(*mut Handle, Handle, *mut RenderParam) -> c_int,
    render_update_callback:
        unsafe extern "C" fn(Handle, Option<unsafe extern "C" fn(*mut c_void)>, *mut c_void),
    render_update: unsafe extern "C" fn(Handle) -> u64,
    render: unsafe extern "C" fn(Handle, *mut RenderParam) -> c_int,
}

#[repr(C)]
struct RawEvent {
    id: c_int,
    error: c_int,
    userdata: u64,
    data: *mut c_void,
}

#[repr(C)]
struct RawEndFile {
    reason: c_int,
    error: c_int,
}

#[repr(C)]
struct RenderParam {
    kind: c_int,
    data: *mut c_void,
}

const RENDER_API_TYPE: c_int = 1;
const RENDER_SW_SIZE: c_int = 17;
const RENDER_SW_FORMAT: c_int = 18;
const RENDER_SW_STRIDE: c_int = 19;
const RENDER_SW_POINTER: c_int = 20;
const UPDATE_FRAME: u64 = 1;
const EVENT_SHUTDOWN: c_int = 1;
const EVENT_END_FILE: c_int = 7;
const EVENT_FILE_LOADED: c_int = 8;
const END_FILE_ERROR: c_int = 4;

impl Mpv {
    // Each lookup's type comes from the field it fills, which is the point of the struct.
    #[allow(clippy::missing_transmute_annotations)]
    fn load() -> Option<Self> {
        // SAFETY: loading a system library by name, and looking up functions of the types declared
        // in its stable public API.
        unsafe {
            let library = libc::dlopen(c"libmpv.so.2".as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
            if library.is_null() {
                return None;
            }
            macro_rules! sym {
                ($name:literal) => {{
                    let found =
                        libc::dlsym(library, concat!($name, "\0").as_ptr() as *const c_char);
                    if found.is_null() {
                        return None;
                    }
                    std::mem::transmute(found)
                }};
            }
            Some(Self {
                create: sym!("mpv_create"),
                set_option: sym!("mpv_set_option_string"),
                initialize: sym!("mpv_initialize"),
                command_string: sym!("mpv_command_string"),
                get_string: sym!("mpv_get_property_string"),
                free: sym!("mpv_free"),
                wait_event: sym!("mpv_wait_event"),
                set_wakeup: sym!("mpv_set_wakeup_callback"),
                observe: sym!("mpv_observe_property"),
                render_create: sym!("mpv_render_context_create"),
                render_update_callback: sym!("mpv_render_context_set_update_callback"),
                render_update: sym!("mpv_render_context_update"),
                render: sym!("mpv_render_context_render"),
            })
        }
    }
}

struct Handles {
    mpv: Mpv,
    core: Handle,
}

// SAFETY: libmpv's client API may be called from any thread.
unsafe impl Send for Handles {}
// SAFETY: as above.
unsafe impl Sync for Handles {}

impl Handles {
    fn command(&self, line: &str) {
        if let Ok(line) = CString::new(line) {
            // SAFETY: a live handle and a valid string. The player's own commands, from a fixed list.
            unsafe { (self.mpv.command_string)(self.core, line.as_ptr()) };
        }
    }

    fn get(&self, name: &CStr) -> Option<String> {
        // SAFETY: a live handle, and libmpv's string is copied and freed as it asks.
        unsafe {
            let raw = (self.mpv.get_string)(self.core, name.as_ptr());
            if raw.is_null() {
                return None;
            }
            let text = CStr::from_ptr(raw).to_string_lossy().into_owned();
            (self.mpv.free)(raw as *mut c_void);
            Some(text)
        }
    }

    fn number(&self, name: &CStr) -> Option<f64> {
        self.get(name)?.parse().ok()
    }
}

struct Wake {
    pending: Mutex<bool>,
    ready: Condvar,
}

impl Wake {
    fn signal(&self) {
        *self.pending.lock().unwrap_or_else(|e| e.into_inner()) = true;
        self.ready.notify_one();
    }
    fn wait(&self) {
        let pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        let (mut pending, _) = self
            .ready
            .wait_timeout_while(pending, Duration::from_millis(500), |p| !*p)
            .unwrap_or_else(|e| e.into_inner());
        *pending = false;
    }
}

unsafe extern "C" fn wake_up(data: *mut c_void) {
    // SAFETY: `data` is the `Wake` that `helper_main` leaks, so it lives for the process.
    unsafe { (*(data as *const Wake)).signal() };
}

/// Runs a player: the jail's own first word has gone, so the arguments are after `ARG`.
pub fn helper_main(args: &[String]) -> ! {
    let parse = |i: usize| args.get(i).and_then(|a| a.parse::<u32>().ok());
    let (Some(w), Some(h)) = (parse(0), parse(1)) else {
        std::process::exit(decode::EXIT_NO_SANDBOX)
    };
    // SAFETY: plain integers. `exit` runs nothing that matters here.
    unsafe {
        libc::prctl(
            libc::PR_SET_PDEATHSIG,
            libc::SIGKILL as libc::c_ulong,
            0,
            0,
            0,
        )
    };
    let Some(mpv) = Mpv::load() else {
        std::process::exit(EXIT_NO_MPV)
    };
    let audio = std::env::var_os("PULSE_SERVER").is_some();
    let handles =
        Arc::new(start_core(mpv, audio).unwrap_or_else(|| std::process::exit(EXIT_CANT_PLAY)));
    let wake: &'static Wake = Box::leak(Box::new(Wake {
        pending: Mutex::new(false),
        ready: Condvar::new(),
    }));
    let wake_data = wake as *const Wake as *mut c_void;
    let mut render: Handle = std::ptr::null_mut();
    let mut create = [
        RenderParam {
            kind: RENDER_API_TYPE,
            data: c"sw".as_ptr() as *mut c_void,
        },
        RenderParam {
            kind: 0,
            data: std::ptr::null_mut(),
        },
    ];
    // SAFETY: a live handle and a list that ends as the API asks. The callbacks' data lives for the
    // process.
    unsafe {
        if (handles.mpv.render_create)(&mut render, handles.core, create.as_mut_ptr()) < 0 {
            std::process::exit(EXIT_CANT_PLAY);
        }
        (handles.mpv.render_update_callback)(render, Some(wake_up), wake_data);
        (handles.mpv.set_wakeup)(handles.core, Some(wake_up), wake_data);
        for name in [
            c"pause",
            c"volume",
            c"mute",
            c"time-pos",
            c"duration",
            c"eof-reached",
        ] {
            (handles.mpv.observe)(handles.core, 0, name.as_ptr(), 0);
        }
    }
    // SAFETY: the app put a file of this size on this descriptor, and it's only written here.
    let shared = unsafe {
        mmap(
            std::ptr::null_mut(),
            SHARED_BYTES,
            ProtFlags::READ | ProtFlags::WRITE,
            MapFlags::SHARED,
            std::os::fd::BorrowedFd::borrow_raw(FD_FRAMES),
            0,
        )
    };
    let Ok(shared) = shared else {
        std::process::exit(EXIT_CANT_PLAY)
    };
    if !lock_down_player() {
        std::process::exit(decode::EXIT_NO_SANDBOX);
    }
    let said = |line: &str| {
        let mut out = std::io::stdout().lock();
        if out.write_all(line.as_bytes()).is_err()
            || out.write_all(b"\n").is_err()
            || out.flush().is_err()
        {
            std::process::exit(0);
        }
    };
    said("hello");
    let size = Arc::new((
        AtomicU32::new(w.clamp(1, MAX_W)),
        AtomicU32::new(h.clamp(1, MAX_H)),
    ));
    let obeyed = Arc::new(AtomicBool::new(false));
    let start = Arc::new(StartGate::default());
    let reader = (handles.clone(), size.clone(), obeyed.clone(), start.clone());
    // The app going away closes this, and the player goes too.
    let spawned = std::thread::Builder::new()
        .name("commands".into())
        .spawn(move || {
            for line in std::io::stdin().lock().lines() {
                let Ok(line) = line else { break };
                obey(&reader.0, &reader.1, &reader.3, &line);
                reader.2.store(true, Ordering::Relaxed);
                wake.signal();
            }
            std::process::exit(0);
        });
    if spawned.is_err() {
        std::process::exit(EXIT_CANT_PLAY);
    }
    handles.command(&format!("loadfile fd://{FD_VIDEO}"));

    let mut next = 0usize;
    let mut last_state = String::new();
    let mut last_report = Instant::now();
    let mut last_resend = Instant::now();
    let mut last_size = (0, 0);
    let mut have_video = false;
    loop {
        wake.wait();
        // SAFETY: a live handle, and each event is read before the next call.
        unsafe {
            loop {
                let event = &*(handles.mpv.wait_event)(handles.core, 0.0);
                match event.id {
                    0 => break,
                    EVENT_SHUTDOWN => std::process::exit(0),
                    EVENT_FILE_LOADED => {
                        have_video = handles.get(c"vid").is_some_and(|v| v != "no");
                        if have_video {
                            start.loaded(&handles);
                        } else {
                            said("novideo");
                        }
                    }
                    EVENT_END_FILE => {
                        let end = &*(event.data as *const RawEndFile);
                        if end.reason == END_FILE_ERROR {
                            said("cantplay");
                        }
                    }
                    _ => {}
                }
            }
        }
        // Asking takes a turn on libmpv's own lock, and a progress bar doesn't need it every frame.
        // A command gets its answer straight away.
        if last_report.elapsed() >= REPORT_EVERY || obeyed.swap(false, Ordering::Relaxed) {
            last_report = Instant::now();
            // Now and then even if nothing's changed, so the app can't stay out of step with it.
            if last_resend.elapsed() >= RESEND_EVERY {
                last_resend = Instant::now();
                last_state.clear();
            }
            let state = format!(
                "state {:.3} {:.3} {} {:.0} {}",
                handles.number(c"time-pos").unwrap_or(0.0).max(0.0),
                handles.number(c"duration").unwrap_or(0.0).max(0.0),
                u8::from(handles.get(c"pause").as_deref() == Some("yes")),
                handles.number(c"volume").unwrap_or(100.0).clamp(0.0, 100.0),
                u8::from(handles.get(c"mute").as_deref() == Some("yes")),
            );
            if state != last_state {
                said(&state);
                last_state = state;
            }
        }
        // SAFETY: a live render context.
        let new_frame = unsafe { (handles.mpv.render_update)(render) } & UPDATE_FRAME != 0;
        let wanted = (
            size.0.load(Ordering::Relaxed),
            size.1.load(Ordering::Relaxed),
        );
        if have_video && (new_frame || wanted != last_size) {
            let Some(natural) = handles
                .number(c"dwidth")
                .zip(handles.number(c"dheight"))
                .filter(|n| n.0 > 0.0 && n.1 > 0.0)
            else {
                continue;
            };
            let scale = (wanted.0 as f64 / natural.0).min(wanted.1 as f64 / natural.1);
            let (fw, fh) = (
                ((natural.0 * scale).round() as u32).clamp(1, MAX_W),
                ((natural.1 * scale).round() as u32).clamp(1, MAX_H),
            );
            let mut render_size = [fw as c_int, fh as c_int];
            let mut stride = fw as usize * 4;
            // SAFETY: the frame fits in its half of the mapping, by the clamps above.
            let pixels = unsafe { (shared as *mut u8).add(next * FRAME_BYTES) };
            let mut params = [
                RenderParam {
                    kind: RENDER_SW_SIZE,
                    data: render_size.as_mut_ptr() as *mut c_void,
                },
                RenderParam {
                    kind: RENDER_SW_FORMAT,
                    data: c"bgr0".as_ptr() as *mut c_void,
                },
                RenderParam {
                    kind: RENDER_SW_STRIDE,
                    data: &mut stride as *mut usize as *mut c_void,
                },
                RenderParam {
                    kind: RENDER_SW_POINTER,
                    data: pixels as *mut c_void,
                },
                RenderParam {
                    kind: 0,
                    data: std::ptr::null_mut(),
                },
            ];
            // SAFETY: a live render context and a list that ends as the API asks.
            if unsafe { (handles.mpv.render)(render, params.as_mut_ptr()) } >= 0 {
                said(&format!("frame {next} {fw} {fh}"));
                next = 1 - next;
                last_size = wanted;
            }
        }
    }
}

fn start_core(mpv: Mpv, audio: bool) -> Option<Handles> {
    // SAFETY: no preconditions, and the result is checked.
    let core = unsafe { (mpv.create)() };
    if core.is_null() {
        return None;
    }
    let handles = Handles { mpv, core };
    let sound_output = ("ao", if audio { "pulse" } else { "null" });
    for (name, value) in PLAYER_OPTIONS.iter().copied().chain([sound_output]) {
        let (name, value) = (CString::new(name).ok()?, CString::new(value).ok()?);
        // SAFETY: the core exists and hasn't been initialized, which is when mpv takes options.
        if unsafe { (handles.mpv.set_option)(core, name.as_ptr(), value.as_ptr()) } < 0 {
            return None;
        }
    }
    // `load-scripts` only covers the user's own scripts. These are mpv's built-in ones, for a window
    // this player doesn't have, and each is an interpreter it has no use for. Their compiler asks
    // for executable memory, which the filter rightly kills. Not every version has all of them, so
    // one that's unknown is fine.
    for name in BUILT_IN_SCRIPTS {
        let Ok(name) = CString::new(*name) else {
            continue;
        };
        // SAFETY: as above.
        unsafe { (handles.mpv.set_option)(core, name.as_ptr(), c"no".as_ptr()) };
    }
    // SAFETY: the core exists, and this is the one time it's initialized.
    (unsafe { (handles.mpv.initialize)(core) } >= 0).then_some(handles)
}

/// How the player is set up, apart from where its sound goes.
const PLAYER_OPTIONS: &[(&str, &str)] = &[
    // Nothing of the user's or left over from last time.
    ("config", "no"),
    ("load-scripts", "no"),
    ("load-auto-profiles", "no"),
    ("resume-playback", "no"),
    ("save-position-on-quit", "no"),
    // Only the one video: no looking for files beside it, or for streams online.
    ("sub-auto", "no"),
    ("sid", "no"),
    ("audio-file-auto", "no"),
    ("cover-art-auto", "no"),
    ("audio-display", "no"),
    ("ytdl", "no"),
    // The app has the controls, so no terminal, key bindings or on-screen display.
    ("terminal", "no"),
    ("input-default-bindings", "no"),
    ("osc", "no"),
    ("osd-level", "0"),
    // Frames made on the CPU, for the app to draw.
    ("hwdec", "no"),
    ("vo", "libmpv"),
    // Still there after the last frame, so the footer's controls keep working.
    ("idle", "yes"),
    ("keep-open", "yes"),
    ("volume-max", "100"),
    // Until it's known there's a picture, so a sound-only file doesn't get to play a moment.
    ("pause", "yes"),
];

const BUILT_IN_SCRIPTS: &[&str] = &[
    "load-stats-overlay",
    "load-console",
    "load-osd-console",
    "load-commands",
    "load-select",
    "load-positioning",
    "load-context-menu",
];

/// The player starts paused until it knows there's a picture, and a press in that time mustn't be
/// forgotten or get ahead of that.
#[derive(Default)]
struct StartGate {
    state: Mutex<(bool, bool)>,
}

impl StartGate {
    /// Records what was asked for, and returns whether it can happen now.
    fn asked(&self, paused: bool) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.1 = paused;
        state.0
    }

    fn loaded(&self, handles: &Handles) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.0 = true;
        if !state.1 {
            handles.command("set pause no");
        }
    }
}

/// Only ever these. Anything else from the pipe is ignored.
fn obey(handles: &Handles, size: &(AtomicU32, AtomicU32), start: &StartGate, line: &str) {
    let mut words = line.split_ascii_whitespace();
    let number = |word: Option<&str>| {
        word.and_then(|w| w.parse::<f64>().ok())
            .filter(|n| n.is_finite())
    };
    match words.next() {
        Some("pause") => {
            if start.asked(true) {
                handles.command("set pause yes");
            }
        }
        Some("play") => {
            if start.asked(false) {
                if handles.get(c"eof-reached").as_deref() == Some("yes") {
                    handles.command("seek 0 absolute");
                }
                handles.command("set pause no");
            }
        }
        Some("mute") => handles.command("set mute yes"),
        Some("unmute") => handles.command("set mute no"),
        Some("seek") => {
            if let Some(by) = number(words.next()) {
                handles.command(&format!("seek {by:.1} relative"));
            }
        }
        Some("seekto") => {
            if let Some(to) = number(words.next()) {
                handles.command(&format!(
                    "seek {:.2} absolute-percent",
                    to.clamp(0.0, 100.0)
                ));
            }
        }
        Some("volume") => {
            if let Some(volume) = number(words.next()) {
                handles.command(&format!("set volume {:.0}", volume.clamp(0.0, 100.0)));
            }
        }
        Some("size") => {
            if let (Some(w), Some(h)) = (number(words.next()), number(words.next())) {
                size.0.store((w as u32).clamp(1, MAX_W), Ordering::Relaxed);
                size.1.store((h as u32).clamp(1, MAX_H), Ordering::Relaxed);
            }
        }
        _ => {}
    }
}

/// Limits and the filter, for after libmpv's libraries are loaded and it has started.
fn lock_down_player() -> bool {
    let limit = |resource, value: u64| {
        let mut l = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: `l` is a valid rlimit that outlives both calls.
        unsafe {
            if libc::getrlimit(resource, &mut l) != 0 {
                return false;
            }
            let value = value.min(l.rlim_cur).min(l.rlim_max);
            l = libc::rlimit {
                rlim_cur: value,
                rlim_max: value,
            };
            libc::setrlimit(resource, &l) == 0
        }
    };
    limit(libc::RLIMIT_CORE, 0)
        && limit(libc::RLIMIT_AS, MAX_MEMORY)
        && decode::sandbox::enter_player()
}

/// For the checks: a real player in the real jail with no window, printing what it says.
#[cfg(feature = "bench")]
pub const BENCH_RUN_ARG: &str = "--video-jail-run";

#[cfg(feature = "bench")]
pub fn bench_run(path: &str, commands: &[String]) -> ! {
    let video = File::open(path).expect("the video");
    let Ok(mut started) = start_jailed(video, (640, 360), &[ARG]) else {
        std::process::exit(99)
    };
    for command in commands {
        started.remote.say(command);
    }
    let cancel = rustix::event::eventfd(0, rustix::event::EventfdFlags::CLOEXEC).expect("eventfd");
    let frames = started.frames;
    let mut lit = false;
    let status = watch(started.child, started.said, &cancel, |said| {
        if let Said::Frame { index, w, h } = said {
            lit |= frames.frame(index, w, h).iter().any(|&p| p & 0xffffff != 0);
            println!("frame {w}x{h} lit={lit}");
        } else {
            println!("{said:?}");
        }
    });
    println!("exit {status:?}");
    std::process::exit(0)
}

/// For the checks: tries one thing as a real jailed, locked-down player.
#[cfg(feature = "bench")]
pub const TRY_ARG: &str = "--video-helper-try";
#[cfg(feature = "bench")]
pub const BENCH_TRY_ARG: &str = "--video-jail-try";

#[cfg(feature = "bench")]
pub fn try_main(what: &str) -> ! {
    if !lock_down_player() {
        std::process::exit(decode::EXIT_NO_SANDBOX);
    }
    std::process::exit(decode::attempt_as_player(what))
}

/// Runs `try_main` through the jail, and exits the way it did.
#[cfg(feature = "bench")]
pub fn bench_try(what: &str) -> ! {
    let executable = std::env::current_exe().expect("own path");
    let status = Command::new("bwrap")
        .args(jail_arguments(&executable, None))
        .args([TRY_ARG, what])
        .status()
        .expect("bubblewrap");
    println!("{status:?}");
    std::process::exit(status.code().unwrap_or(128 + status.signal().unwrap_or(0)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sensible_lines_parse() {
        assert_eq!(parse_said("hello"), Some(Said::Hello));
        assert_eq!(
            parse_said("frame 1 640 360"),
            Some(Said::Frame {
                index: 1,
                w: 640,
                h: 360
            })
        );
        assert_eq!(
            parse_said("state 1.5 10 1 50 0"),
            Some(Said::State(PlayState {
                position: 1.5,
                duration: 10.0,
                paused: true,
                volume: 50.0,
                muted: false
            }))
        );
        assert_eq!(parse_said("novideo"), Some(Said::NoVideo));
    }

    #[test]
    fn a_player_cant_make_the_app_read_past_its_frames() {
        for line in [
            "frame 2 10 10",
            "frame 0 3841 10",
            "frame 0 10 2161",
            "frame 0 0 10",
            "frame 0 10 0",
            "frame 0 4294967295 4294967295",
            "frame -1 10 10",
            "frame 0 10",
        ] {
            assert_eq!(parse_said(line), None, "{line}");
        }
    }

    #[test]
    fn nonsense_from_a_player_is_ignored() {
        for line in [
            "",
            "exec /bin/sh",
            "hello there",
            "state nan 1 0 1 0",
            "state inf 1 0 1 0",
            "state -1 1 0 1 0",
            "state 1 1 0",
            "frame 0 10 10 extra",
        ] {
            assert_eq!(parse_said(line), None, "{line}");
        }
    }

    #[test]
    fn volume_from_a_player_stays_in_range() {
        let Some(Said::State(state)) = parse_said("state 0 0 0 900 0") else {
            panic!()
        };
        assert_eq!(state.volume, 100.0);
    }

    #[test]
    fn video_files_are_told_from_other_things() {
        assert!(looks_like_video(b"\0\0\0\x20ftypisom"));
        assert!(looks_like_video(&[0x1a, 0x45, 0xdf, 0xa3, 0]));
        assert!(looks_like_video(b"RIFF\0\0\0\0AVI LIST"));
        assert!(!looks_like_video(b"RIFF\0\0\0\0WAVEfmt "));
        assert!(looks_like_video(b"\0\0\0\x14ftypqt  "), "a .mov");
        assert!(
            looks_like_video(b"\0\0\0\x08wide\0\0\0\0mdat"),
            "an old .mov"
        );
        assert!(!looks_like_video(b"\0\0\0\x18ftypheic"), "a phone photo");
        assert!(!looks_like_video(b"\0\0\0\x18ftypavif"));
        assert!(!looks_like_video(b"\0\0\0\x18ftypmif1"));
        assert!(!looks_like_video(b"FLV is a format, says this note"));
        assert!(
            !looks_like_video(b"The free software movement"),
            "words that look like atoms"
        );
        assert!(!looks_like_video(b"Our wide range of goods"));
        assert!(
            !looks_like_video(b"\0\0\0\x18ftypcrx "),
            "a Canon raw photo"
        );
        let mut stream = vec![0u8; 400];
        (stream[0], stream[188], stream[376]) = (0x47, 0x47, 0x47);
        assert!(looks_like_video(&stream), "a transport stream");
        stream[376] = 0;
        assert!(!looks_like_video(&stream), "a G here and there isn't one");
        assert!(!looks_like_video(b"just some words"));
        assert!(!looks_like_video(b""));
    }

    fn player(script: &str) -> (Child, File) {
        let (read, write) = pipe_with(PipeFlags::CLOEXEC).unwrap();
        let child = Command::new("sh")
            .args(["-c", script])
            .stdout(Stdio::from(write))
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        (child, File::from(read))
    }

    fn cancel_fd() -> OwnedFd {
        rustix::event::eventfd(
            0,
            rustix::event::EventfdFlags::CLOEXEC | rustix::event::EventfdFlags::NONBLOCK,
        )
        .unwrap()
    }

    fn cancel_after(cancel: &OwnedFd, wait: Duration) {
        let cancel = cancel.try_clone().unwrap();
        std::thread::spawn(move || {
            std::thread::sleep(wait);
            rustix::io::write(&cancel, &1u64.to_ne_bytes()).unwrap();
        });
    }

    /// Fails rather than hangs the test run if `watch` never returns.
    fn watched(
        script: &str,
        cancel: Option<Duration>,
    ) -> (Option<ExitStatus>, Vec<Said>, Duration) {
        let (tx, rx) = std::sync::mpsc::channel();
        let (child, said) = player(script);
        let token = cancel_fd();
        if let Some(wait) = cancel {
            cancel_after(&token, wait);
        }
        std::thread::spawn(move || {
            let start = Instant::now();
            let mut heard = Vec::new();
            let status = watch(child, said, &token, |s| heard.push(s));
            let _ = tx.send((status, heard, start.elapsed()));
        });
        rx.recv_timeout(Duration::from_secs(20))
            .expect("watch came back")
    }

    #[test]
    fn a_normal_exit_is_seen_with_its_code_and_everything_said() {
        let (status, heard, _) = watched("echo hello; echo frame 0 10 10; exit 8", None);
        assert_eq!(status.unwrap().code(), Some(8));
        assert!(
            heard.contains(&Said::Hello)
                && heard.contains(&Said::Frame {
                    index: 0,
                    w: 10,
                    h: 10
                }),
            "{heard:?}"
        );
    }

    #[test]
    fn closing_its_output_and_carrying_on_doesnt_hide_a_player() {
        let (status, _, took) = watched("exec 1>&-; exec sleep 30", None);
        assert_eq!(
            status.unwrap().signal(),
            Some(libc::SIGKILL),
            "it has nothing left to do"
        );
        assert!(took < Duration::from_secs(5));
    }

    #[test]
    fn cancelling_works_after_its_output_has_closed() {
        let (status, _, took) =
            watched("exec 1>&-; exec sleep 30", Some(Duration::from_millis(50)));
        assert!(status.is_none());
        assert!(took < Duration::from_secs(5));
    }

    #[test]
    fn bytes_that_arent_text_are_skipped_and_it_keeps_listening() {
        let (status, heard, _) = watched("printf '\\377\\376\\n'; echo hello; exit 3", None);
        assert_eq!(status.unwrap().code(), Some(3));
        assert_eq!(heard, [Said::Hello]);
    }

    #[test]
    fn a_line_with_no_end_costs_nothing_and_later_lines_still_count() {
        // Half a gigabyte with no newline, then a real line. Memory is limited so this can't pass by luck.
        let script = "ulimit -v 200000; head -c 500000000 /dev/zero; echo; echo hello; exit 3";
        let (status, heard, _) = watched(script, None);
        assert_eq!(status.unwrap().code(), Some(3));
        assert_eq!(heard, [Said::Hello]);
    }

    #[test]
    fn a_flood_is_cut_down_to_a_steady_rate_and_can_still_be_cancelled() {
        let (status, heard, took) = watched("yes 'frame 0 1 1'", Some(Duration::from_millis(500)));
        assert!(status.is_none());
        assert!(took < Duration::from_secs(5), "{took:?}");
        assert!(
            !heard.is_empty() && heard.len() < 150,
            "{} deliveries in half a second",
            heard.len()
        );
    }

    #[test]
    fn endings_are_told_apart_even_through_bubblewrap() {
        use std::os::unix::process::ExitStatusExt as _;
        let exit = |code: i32| ExitStatus::from_raw(code << 8);
        assert_eq!(
            ending(exit(128 + libc::SIGSYS), true),
            Ending::SandboxStopped,
            "how bubblewrap reports it"
        );
        assert_eq!(
            ending(ExitStatus::from_raw(libc::SIGSYS), false),
            Ending::SandboxStopped
        );
        assert_eq!(ending(exit(EXIT_NO_MPV), false), Ending::NoMpv);
        assert_eq!(
            ending(exit(EXIT_CANT_PLAY), false),
            Ending::PlayerFailed,
            "not the jail's fault"
        );
        assert_eq!(
            ending(exit(decode::EXIT_NO_SANDBOX), false),
            Ending::NoSandbox
        );
        assert_eq!(ending(exit(1), false), Ending::JailFailed);
        assert_eq!(
            ending(exit(128 + libc::SIGSEGV), false),
            Ending::PlayerFailed,
            "a crash isn't the jail's fault"
        );
        assert_eq!(ending(exit(0), true), Ending::Other);
    }

    #[test]
    fn the_jail_shows_what_it_should_and_no_more() {
        let words = jail_arguments(Path::new("/opt/gannet"), None);
        let text = words.join(" ");
        assert!(
            text.contains("--unshare-all")
                && text.contains("--die-with-parent")
                && text.contains("--clearenv")
                && text.contains("--cap-drop ALL")
        );
        assert!(text.contains("--ro-bind /opt/gannet /gannet-helper"));
        assert!(!text.contains("PULSE_SERVER"), "no socket, no sound");
        assert!(!text.contains(" /home") && !text.contains("/run/user") && !text.contains("/proc"));
        assert_eq!(words.last().unwrap(), JAILED_SELF);
        let position = |word: &str| words.iter().position(|w| w == word).unwrap();
        assert!(
            position("--clearenv") < position("--setenv"),
            "clearing the environment after setting it would undo it"
        );
        let with_sound = jail_arguments(
            Path::new("/opt/gannet"),
            Some(Path::new("/run/user/1000/pulse/native")),
        )
        .join(" ");
        assert!(with_sound.contains("--ro-bind /run/user/1000/pulse/native /sound-server"));
        assert!(with_sound.contains("--setenv PULSE_SERVER unix:/sound-server"));
    }
}
