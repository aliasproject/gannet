//! The decoder process. Image decoders read whatever files people have, so they run in a
//! short-lived copy of this program: a crash or a hostile file stays out of the app, stopping
//! a decode is just killing it, and the memory goes back when it exits.
//!
//! It reads the file on stdin, writes the pixels to stdout, and answers with its exit code.
//! It locks itself down before it reads a byte (see `lock_down`).

use std::fs::File;
use std::io::{BufReader, Read, Seek, Write};
use std::os::fd::FromRawFd;

use crate::doc;
use image::codecs::gif::GifDecoder;
use image::codecs::png::PngDecoder;
use image::codecs::webp::WebPDecoder;
use image::error::ImageError;
use image::metadata::Orientation;
use image::{AnimationDecoder, DynamicImage, ImageDecoder, ImageFormat, ImageReader, Limits};

/// The argument that makes this program a decoder rather than a window.
pub const ARG: &str = "--preview-helper";
/// After `ARG`: read the file as text, or as Markdown, rather than as an image.
pub const TEXT_ARG: &str = "text";
pub const MARKDOWN_ARG: &str = "markdown";

pub const EXIT_UNSUPPORTED: i32 = 3;
pub const EXIT_DAMAGED: i32 = 4;
pub const EXIT_TOO_LARGE: i32 = 5;
pub const EXIT_NO_SANDBOX: i32 = 6;

/// What comes before the pixels: `MAGIC`, then width and height as native-endian `u32`s.
pub const MAGIC: &[u8; 4] = b"GNPV";
pub const HEADER: usize = 12;
/// An animation starts with this in `MAGIC`'s place, then the same width and height. Its frames
/// follow one after another, whole, then each one's time on screen in milliseconds, then how many
/// frames there are, all as native-endian `u32`s.
pub const ANIMATION_MAGIC: &[u8; 4] = b"GNPA";
/// More than this and an animation is cut short, as it is when all its frames would come to more
/// pixels than one image may have.
pub const MAX_FRAMES: u32 = 2000;

const GIB: u64 = 1024 * 1024 * 1024;
/// Past this, a photo is a poster scan or a trap. At 4 bytes a pixel, this is 600 MB.
const MAX_PIXELS: u64 = 150_000_000;
const MAX_SIDE: u32 = 65_535;
const MAX_ALLOC: u64 = GIB;
const MAX_ADDRESS_SPACE: u64 = 3 * GIB;
const MAX_CPU_SECONDS: u64 = 20;

#[derive(Debug, PartialEq, Eq)]
pub enum Failure {
    Unsupported,
    Damaged,
    TooLarge,
}

pub fn helper_main(mode: Option<&str>) -> ! {
    if !lock_down() {
        std::process::exit(EXIT_NO_SANDBOX);
    }
    // SAFETY: the app gave us these three descriptors, and nothing else here uses them.
    let (input, output) = unsafe { (File::from_raw_fd(0), File::from_raw_fd(1)) };
    let done = match mode {
        Some(TEXT_ARG) => document(input, output, false),
        Some(MARKDOWN_ARG) => document(input, output, true),
        _ => decode(input, output),
    };
    std::process::exit(match done {
        Ok(()) => 0,
        Err(Failure::Unsupported) => EXIT_UNSUPPORTED,
        Err(Failure::Damaged) => EXIT_DAMAGED,
        Err(Failure::TooLarge) => EXIT_TOO_LARGE,
    })
}

/// Limits what a decode can cost and what code that took over this process could do. Returns
/// false if any of it couldn't be set up, and then nothing must be decoded.
fn lock_down() -> bool {
    // Never a raise, which isn't allowed: a stricter limit from whatever started the app stands.
    let limit = |resource, value: u64| {
        let mut l = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
        // SAFETY: `l` is a valid rlimit that outlives both calls.
        unsafe {
            if libc::getrlimit(resource, &mut l) != 0 {
                return false;
            }
            let value = value.min(l.rlim_cur).min(l.rlim_max);
            l = libc::rlimit { rlim_cur: value, rlim_max: value };
            libc::setrlimit(resource, &l) == 0
        }
    };
    // SAFETY: these calls take plain integers and touch nothing of ours but descriptors we
    // don't use.
    let closed = unsafe {
        libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL as libc::c_ulong, 0, 0, 0);
        libc::close_range(3, libc::c_uint::MAX, 0) == 0
    };
    closed
        && limit(libc::RLIMIT_NOFILE, 3)
        && limit(libc::RLIMIT_AS, MAX_ADDRESS_SPACE)
        && limit(libc::RLIMIT_CPU, MAX_CPU_SECONDS)
        && limit(libc::RLIMIT_FSIZE, MAX_PIXELS * 4 + HEADER as u64 + 4 * (MAX_FRAMES as u64 + 1))
        && limit(libc::RLIMIT_CORE, 0)
        // Last, as it forbids almost everything above.
        && sandbox::enter()
}

/// The argument for trying something forbidden in a real locked-down decoder, for the checks.
#[cfg(feature = "bench")]
pub const TRY_ARG: &str = "--preview-helper-try";

#[cfg(feature = "bench")]
pub fn try_main(what: &str) -> ! {
    if !lock_down() {
        std::process::exit(EXIT_NO_SANDBOX);
    }
    std::process::exit(attempt(what))
}

/// Tries one thing as a jailed player. Paths are refused by the jail here, not the filter, so what
/// counts is whether they're there at all.
#[cfg(feature = "bench")]
pub fn attempt_as_player(what: &str) -> i32 {
    let exists = |p: &str| std::path::Path::new(p).exists();
    if let Some(path) = what.strip_prefix("see:") {
        return if exists(path) { 1 } else { REFUSED };
    }
    match what {
        "see-etc" => if exists("/etc/passwd") { 1 } else { REFUSED },
        "write-usr" => if std::fs::File::create("/usr/gannet-was-here").is_ok() { 1 } else { REFUSED },
        // SAFETY: only ever run in a process that's about to exit. The call has no pointers.
        "network" => if unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) } < 0 { REFUSED } else { 1 },
        "thread" => match std::thread::Builder::new().spawn(|| {}) {
            Ok(thread) => if thread.join().is_ok() { REACHED_THE_END } else { 1 },
            Err(_) => 1,
        },
        other => attempt(other),
    }
}

/// What `attempt` returns if the call came back with a refusal, and if nothing stopped it.
#[cfg(any(test, feature = "bench"))]
const REFUSED: i32 = 40;
#[cfg(any(test, feature = "bench"))]
const REACHED_THE_END: i32 = 41;

/// Tries one thing once locked down. Only ever run in a process that's about to exit, and only
/// by the checks. The dangerous ones touch nothing even if they get through.
#[cfg(any(test, feature = "bench"))]
fn attempt(what: &str) -> i32 {
    let refused = |result: libc::c_long| result < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ENOSYS);
    let code = |result: libc::c_long| if refused(result) { REFUSED } else { 1 };
    let path = c"/etc/passwd".as_ptr();
    // SAFETY: as above. Each call either fails or, as is the point, gets the process killed.
    unsafe {
        match what {
            "nothing" => REACHED_THE_END,
            "open" => code(libc::openat(libc::AT_FDCWD, path, libc::O_RDONLY) as libc::c_long),
            "stat-path" => code(libc::syscall(libc::SYS_newfstatat, libc::AT_FDCWD, path, std::ptr::null_mut::<libc::stat>(), 0)),
            "statx-path" => code(libc::syscall(libc::SYS_statx, libc::AT_FDCWD, path, 0, 0, std::ptr::null_mut::<libc::c_void>())),
            "fstat-is-fine" => {
                let mut st: libc::stat = std::mem::zeroed();
                if libc::fstat(0, &mut st) == 0 { REACHED_THE_END } else { 1 }
            }
            "socket" => {
                libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0);
                REACHED_THE_END
            }
            "exec" => {
                let args = [c"/bin/true".as_ptr(), std::ptr::null()];
                libc::execv(c"/bin/true".as_ptr(), args.as_ptr());
                REACHED_THE_END
            }
            "fork" => {
                libc::syscall(libc::SYS_clone, libc::SIGCHLD, 0, 0, 0, 0);
                REACHED_THE_END
            }
            "thread" => {
                let _ = std::thread::Builder::new().spawn(|| {});
                REACHED_THE_END
            }
            "exec-memory" => {
                libc::mmap(std::ptr::null_mut(), 4096, libc::PROT_READ | libc::PROT_EXEC, libc::MAP_PRIVATE | libc::MAP_ANONYMOUS, -1, 0);
                REACHED_THE_END
            }
            "make-executable" => {
                let page = libc::mmap(std::ptr::null_mut(), 4096, libc::PROT_READ, libc::MAP_PRIVATE | libc::MAP_ANONYMOUS, -1, 0);
                libc::mprotect(page, 4096, libc::PROT_READ | libc::PROT_EXEC);
                REACHED_THE_END
            }
            "signal-another" => {
                // Signal 0 only checks that the process exists.
                libc::syscall(libc::SYS_tgkill, 1, 1, 0);
                REACHED_THE_END
            }
            "out-of-memory" => libc::abort(),
            other => panic!("unknown attempt {other}"),
        }
    }
}

/// A seccomp allowlist: what decoding needs, and any other system call kills the process. So
/// nothing here can make a socket, start a program or make memory executable. Opening a file or
/// looking one up by path fails rather than kills, because the C library and Rust's standard
/// library sometimes try and cope when they can't, and a good image shouldn't die for that.
///
/// Seccomp covers only the calling thread and those it starts, and this process has only one
/// thread.
///
/// What this doesn't stop: code that took over this process can still read the one file it was
/// given, write to its one output, and use up its CPU and memory limits. The filter also doesn't
/// stop that code from running at all. It limits what it can do.
pub(crate) mod sandbox {
    #[cfg(target_arch = "x86_64")]
    const ARCH: Option<u32> = Some(0xc000_003e); // AUDIT_ARCH_X86_64
    #[cfg(target_arch = "aarch64")]
    const ARCH: Option<u32> = Some(0xc000_00b7); // AUDIT_ARCH_AARCH64
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    const ARCH: Option<u32> = None;

    const ALLOWED: &[libc::c_long] = &[
        libc::SYS_read,
        libc::SYS_write,
        libc::SYS_lseek,
        libc::SYS_close,
        libc::SYS_fstat,
        libc::SYS_brk,
        libc::SYS_munmap,
        libc::SYS_mremap,
        libc::SYS_madvise,
        libc::SYS_rt_sigaction,
        libc::SYS_rt_sigprocmask,
        libc::SYS_rt_sigreturn,
        libc::SYS_sigaltstack,
        libc::SYS_futex,
        libc::SYS_getrandom,
        libc::SYS_sched_yield,
        libc::SYS_clock_gettime,
        libc::SYS_getpid,
        libc::SYS_gettid,
        libc::SYS_exit,
        libc::SYS_exit_group,
    ];

    /// These take a path, and a filter can't see what a path says. Opening is refused for good
    /// reason, and looking up says whether a file exists, its size and when it changed. `ENOSYS`
    /// makes Rust's standard library fall back to `fstat`, which only takes a descriptor.
    #[cfg(target_arch = "x86_64")]
    const BY_PATH: &[libc::c_long] = &[libc::SYS_open, libc::SYS_openat, libc::SYS_openat2, libc::SYS_newfstatat, libc::SYS_statx];
    #[cfg(not(target_arch = "x86_64"))]
    const BY_PATH: &[libc::c_long] = &[libc::SYS_openat, libc::SYS_openat2, libc::SYS_newfstatat, libc::SYS_statx];

    const LD_ABS_W: u16 = 0x20; // BPF_LD | BPF_W | BPF_ABS
    const JEQ_K: u16 = 0x15; // BPF_JMP | BPF_JEQ | BPF_K
    const JSET_K: u16 = 0x45; // BPF_JMP | BPF_JSET | BPF_K
    const RET_K: u16 = 0x06; // BPF_RET | BPF_K
    const RET_ALLOW: u32 = 0x7fff_0000;
    const RET_KILL_PROCESS: u32 = 0x8000_0000;
    const RET_ERRNO: u32 = 0x0005_0000;
    // Where things are in the kernel's `seccomp_data`.
    const NR: u32 = 0;
    const AUDIT_ARCH: u32 = 4;
    const fn arg(i: u32) -> u32 {
        16 + 8 * i
    }

    fn op(code: u16, jt: u8, jf: u8, k: u32) -> libc::sock_filter {
        libc::sock_filter { code, jt, jf, k }
    }

    /// `pid` is who `tgkill` may signal: this process, for what `abort` does when memory runs
    /// out. Signalling any other process is forbidden.
    fn program(arch: u32, pid: u32) -> Vec<libc::sock_filter> {
        let mut p = vec![
            op(LD_ABS_W, 0, 0, AUDIT_ARCH),
            op(JEQ_K, 1, 0, arch),
            op(RET_K, 0, 0, RET_KILL_PROCESS),
            op(LD_ABS_W, 0, 0, NR),
        ];
        for &nr in BY_PATH {
            p.push(op(JEQ_K, 0, 1, nr as u32));
            p.push(op(RET_K, 0, 0, RET_ERRNO | libc::ENOSYS as u32));
        }
        for &nr in ALLOWED {
            p.push(op(JEQ_K, 0, 1, nr as u32));
            p.push(op(RET_K, 0, 0, RET_ALLOW));
        }
        // Memory, as long as it isn't executable.
        for nr in [libc::SYS_mmap, libc::SYS_mprotect] {
            p.push(op(JEQ_K, 0, 4, nr as u32));
            p.push(op(LD_ABS_W, 0, 0, arg(2)));
            p.push(op(JSET_K, 0, 1, libc::PROT_EXEC as u32));
            p.push(op(RET_K, 0, 0, RET_KILL_PROCESS));
            p.push(op(RET_K, 0, 0, RET_ALLOW));
        }
        p.push(op(JEQ_K, 0, 3, libc::SYS_tgkill as u32));
        p.push(op(LD_ABS_W, 0, 0, arg(0)));
        p.push(op(JEQ_K, 0, 1, pid));
        p.push(op(RET_K, 0, 0, RET_ALLOW));
        p.push(op(RET_K, 0, 0, RET_KILL_PROCESS));
        p
    }

    pub fn enter() -> bool {
        let Some(arch) = ARCH else { return false };
        install(&program(arch, std::process::id()))
    }

    /// For the video player, which is in a jail already and needs far more than a decoder: its
    /// threads, its libraries and a sound socket. So this names what's forbidden, and everything
    /// else goes through. Forked processes and new executable memory die, and nothing but a local
    /// socket can be made. Libraries are all loaded before this goes on.
    pub fn enter_player() -> bool {
        let Some(arch) = ARCH else { return false };
        install(&player_program(arch))
    }

    const JGE_K: u16 = 0x35; // BPF_JMP | BPF_JGE | BPF_K
    const X32_BIT: u32 = 0x4000_0000;

    #[cfg(target_arch = "x86_64")]
    const FORBIDDEN_HERE: &[libc::c_long] = &[libc::SYS_fork, libc::SYS_vfork, libc::SYS_iopl, libc::SYS_ioperm];
    #[cfg(not(target_arch = "x86_64"))]
    const FORBIDDEN_HERE: &[libc::c_long] = &[];

    const FORBIDDEN: &[libc::c_long] = &[
        libc::SYS_execve,
        libc::SYS_execveat,
        libc::SYS_ptrace,
        libc::SYS_process_vm_readv,
        libc::SYS_process_vm_writev,
        libc::SYS_kcmp,
        libc::SYS_mount,
        libc::SYS_umount2,
        libc::SYS_pivot_root,
        libc::SYS_chroot,
        libc::SYS_setns,
        libc::SYS_unshare,
        libc::SYS_init_module,
        libc::SYS_finit_module,
        libc::SYS_delete_module,
        libc::SYS_kexec_load,
        libc::SYS_reboot,
        libc::SYS_swapon,
        libc::SYS_swapoff,
        libc::SYS_bpf,
        libc::SYS_perf_event_open,
        libc::SYS_userfaultfd,
        libc::SYS_io_uring_setup,
        libc::SYS_keyctl,
        libc::SYS_add_key,
        libc::SYS_request_key,
        libc::SYS_open_by_handle_at,
        libc::SYS_name_to_handle_at,
        libc::SYS_personality,
        libc::SYS_pkey_mprotect,
        libc::SYS_shmat,
        libc::SYS_pidfd_getfd,
        libc::SYS_process_madvise,
        libc::SYS_fanotify_init,
        libc::SYS_acct,
        libc::SYS_quotactl,
    ];

    fn player_program(arch: u32) -> Vec<libc::sock_filter> {
        const CLONE_THREAD: u32 = 0x0001_0000;
        let mut p = vec![
            op(LD_ABS_W, 0, 0, AUDIT_ARCH),
            op(JEQ_K, 1, 0, arch),
            op(RET_K, 0, 0, RET_KILL_PROCESS),
            op(LD_ABS_W, 0, 0, NR),
            // A list of what's forbidden can't cover the other numbering of the same calls.
            op(JGE_K, 0, 1, X32_BIT),
            op(RET_K, 0, 0, RET_KILL_PROCESS),
        ];
        for &nr in FORBIDDEN.iter().chain(FORBIDDEN_HERE) {
            p.push(op(JEQ_K, 0, 1, nr as u32));
            p.push(op(RET_K, 0, 0, RET_KILL_PROCESS));
        }
        // A filter can't read clone3's flags, and this makes the C library use plain clone.
        p.push(op(JEQ_K, 0, 1, libc::SYS_clone3 as u32));
        p.push(op(RET_K, 0, 0, RET_ERRNO | libc::ENOSYS as u32));
        // Threads, but no new processes.
        p.push(op(JEQ_K, 0, 3, libc::SYS_clone as u32));
        p.push(op(LD_ABS_W, 0, 0, arg(0)));
        p.push(op(JSET_K, 1, 0, CLONE_THREAD));
        p.push(op(RET_K, 0, 0, RET_KILL_PROCESS));
        // That falls through with the wrong value loaded, and everything from here checks for it.
        p.push(op(LD_ABS_W, 0, 0, NR));
        for nr in [libc::SYS_mmap, libc::SYS_mprotect] {
            p.push(op(JEQ_K, 0, 4, nr as u32));
            p.push(op(LD_ABS_W, 0, 0, arg(2)));
            p.push(op(JSET_K, 0, 1, libc::PROT_EXEC as u32));
            p.push(op(RET_K, 0, 0, RET_KILL_PROCESS));
            p.push(op(RET_K, 0, 0, RET_ALLOW));
        }
        // Only local sockets. There's no network in the jail anyway.
        p.push(op(JEQ_K, 0, 3, libc::SYS_socket as u32));
        p.push(op(LD_ABS_W, 0, 0, arg(0)));
        p.push(op(JEQ_K, 1, 0, libc::AF_UNIX as u32));
        p.push(op(RET_K, 0, 0, RET_ERRNO | libc::EACCES as u32));
        p.push(op(RET_K, 0, 0, RET_ALLOW));
        p
    }

    fn install(program: &[libc::sock_filter]) -> bool {
        let Ok(len) = u16::try_from(program.len()) else { return false };
        let fprog = libc::sock_fprog { len, filter: program.as_ptr() as *mut libc::sock_filter };
        // The player has threads by the time this goes on, and all of them need it.
        const SECCOMP_FILTER_FLAG_TSYNC: libc::c_ulong = 1;
        // SAFETY: `fprog` points at a valid program that outlives the call, and the kernel copies
        // it. The other arguments are plain integers.
        unsafe {
            libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1 as libc::c_ulong, 0, 0, 0) == 0
                && libc::syscall(libc::SYS_seccomp, 1 as libc::c_ulong, SECCOMP_FILTER_FLAG_TSYNC, &fprog as *const libc::sock_fprog) == 0
        }
    }
}

fn classify(e: ImageError) -> Failure {
    match e {
        ImageError::Limits(_) => Failure::TooLarge,
        ImageError::Unsupported(_) => Failure::Unsupported,
        _ => Failure::Damaged,
    }
}

/// Reads the start of a text file and writes it out as a document, laid out as Markdown or as it
/// is. A file with a zero byte near the start isn't text, which is `Unsupported`.
pub fn document(input: impl Read, mut output: impl Write, markdown: bool) -> Result<(), Failure> {
    let mut raw = Vec::with_capacity(doc::TEXT_LIMIT + 1);
    // One byte past the limit says whether there's more.
    input.take(doc::TEXT_LIMIT as u64 + 1).read_to_end(&mut raw).map_err(|_| Failure::Damaged)?;
    let (text, cut) = doc::excerpt(&raw).ok_or(Failure::Unsupported)?;
    // Markdown that's all HTML would show nothing, so it shows as it is instead.
    let doc = match markdown.then(|| doc::markdown(&text, cut)) {
        Some(doc) if !doc.blocks.is_empty() => doc,
        _ => doc::plain(text, cut),
    };
    output.write_all(&doc::encode(&doc)).map_err(|_| Failure::TooLarge)
}

fn limits() -> Limits {
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_SIDE);
    limits.max_image_height = Some(MAX_SIDE);
    limits.max_alloc = Some(MAX_ALLOC);
    limits
}

/// Decodes any image `input` holds, upright, and writes it out as straight RGBA. One that moves
/// is written as an animation.
pub fn decode<R: Read + Seek>(input: R, mut output: impl Write) -> Result<(), Failure> {
    let reader = ImageReader::new(BufReader::new(input)).with_guessed_format().map_err(|_| Failure::Damaged)?;
    let Some(format) = reader.format() else { return Err(Failure::Unsupported) };
    let mut input = reader.into_inner();
    if let Some(frames) = animation(format, &mut input)? {
        return write_animation(frames, &mut output);
    }
    input.rewind().map_err(|_| Failure::Damaged)?;
    let mut reader = ImageReader::with_format(input, format);
    reader.limits(limits());
    let mut decoder = reader.into_decoder().map_err(classify)?;
    let (w, h) = decoder.dimensions();
    if w as u64 * h as u64 > MAX_PIXELS {
        return Err(Failure::TooLarge);
    }
    // Phones save photos on their side and record which way is up.
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let mut image = DynamicImage::from_decoder(decoder).map_err(classify)?;
    image.apply_orientation(orientation);
    let rgba = image.into_rgba8();
    let mut header = MAGIC.to_vec();
    header.extend_from_slice(&rgba.width().to_ne_bytes());
    header.extend_from_slice(&rgba.height().to_ne_bytes());
    output.write_all(&header).map_err(|_| Failure::TooLarge)?;
    output.write_all(rgba.as_raw()).map_err(|_| Failure::TooLarge)?;
    Ok(())
}

/// The frames of an image that moves, or `None` if it's a still one. Leaves `input` somewhere to
/// rewind from either way.
fn animation<'a, R: Read + Seek + 'a>(format: ImageFormat, input: &'a mut BufReader<R>) -> Result<Option<image::Frames<'a>>, Failure> {
    input.rewind().map_err(|_| Failure::Damaged)?;
    let frames = match format {
        // Every GIF can move. One with a single frame is written as one anyway, which costs nothing.
        ImageFormat::Gif => {
            let mut decoder = GifDecoder::new(input).map_err(classify)?;
            decoder.set_limits(limits()).map_err(classify)?;
            decoder.into_frames()
        }
        ImageFormat::WebP => {
            let mut decoder = WebPDecoder::new(input).map_err(classify)?;
            if !decoder.has_animation() {
                return Ok(None);
            }
            decoder.set_limits(limits()).map_err(classify)?;
            decoder.into_frames()
        }
        ImageFormat::Png => {
            let mut decoder = PngDecoder::new(input).map_err(classify)?;
            if !decoder.is_apng().map_err(classify)? {
                return Ok(None);
            }
            decoder.set_limits(limits()).map_err(classify)?;
            decoder.apng().map_err(classify)?.into_frames()
        }
        _ => return Ok(None),
    };
    Ok(Some(frames))
}

/// Writes frames out as they're decoded, so no more than two are ever held here. A frame that fails
/// after the first ends the animation there, which shows what could be read, and one that turns out
/// to have a single frame is written as a still image.
fn write_animation(frames: image::Frames, output: &mut impl Write) -> Result<(), Failure> {
    let header = |magic: &[u8; 4], w: u32, h: u32| [&magic[..], &w.to_ne_bytes(), &h.to_ne_bytes()].concat();
    let mut first: Option<image::RgbaImage> = None;
    let mut delays: Vec<u32> = Vec::new();
    let mut pixels = 0u64;
    for frame in frames {
        let frame = match frame {
            Ok(frame) => frame,
            Err(e) if first.is_none() => return Err(classify(e)),
            Err(_) => break,
        };
        let (numerator, denominator) = frame.delay().numer_denom_ms();
        let buffer = frame.into_buffer();
        let (w, h) = buffer.dimensions();
        let Some(held) = &first else {
            if w == 0 || h == 0 || w as u64 * h as u64 > MAX_PIXELS {
                return Err(Failure::TooLarge);
            }
            pixels = w as u64 * h as u64;
            delays.push(numerator.checked_div(denominator).unwrap_or(0));
            first = Some(buffer);
            continue;
        };
        if buffer.dimensions() != held.dimensions() || delays.len() as u32 >= MAX_FRAMES || pixels + w as u64 * h as u64 > MAX_PIXELS {
            break;
        }
        // The second frame: it moves, so the first goes out as an animation's.
        if delays.len() == 1 {
            output.write_all(&header(ANIMATION_MAGIC, w, h)).map_err(|_| Failure::TooLarge)?;
            output.write_all(held.as_raw()).map_err(|_| Failure::TooLarge)?;
        }
        pixels += w as u64 * h as u64;
        output.write_all(buffer.as_raw()).map_err(|_| Failure::TooLarge)?;
        delays.push(numerator.checked_div(denominator).unwrap_or(0));
    }
    let Some(first) = first else { return Err(Failure::Damaged) };
    if delays.len() == 1 {
        output.write_all(&header(MAGIC, first.width(), first.height())).map_err(|_| Failure::TooLarge)?;
        return output.write_all(first.as_raw()).map_err(|_| Failure::TooLarge);
    }
    let mut trailer: Vec<u8> = delays.iter().flat_map(|d| d.to_ne_bytes()).collect();
    trailer.extend_from_slice(&(delays.len() as u32).to_ne_bytes());
    output.write_all(&trailer).map_err(|_| Failure::TooLarge)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::codecs::jpeg::JpegEncoder;
    use image::{ImageEncoder, RgbImage, RgbaImage};
    use std::io::Cursor;

    fn run(bytes: &[u8]) -> Result<(u32, u32, Vec<u8>), Failure> {
        let mut out = Vec::new();
        decode(Cursor::new(bytes), &mut out)?;
        assert_eq!(&out[..4], MAGIC);
        let w = u32::from_ne_bytes(out[4..8].try_into().unwrap());
        let h = u32::from_ne_bytes(out[8..12].try_into().unwrap());
        assert_eq!(out.len(), HEADER + w as usize * h as usize * 4);
        Ok((w, h, out[HEADER..].to_vec()))
    }

    fn encoded(image: &RgbaImage, format: image::ImageFormat) -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        image.write_to(&mut out, format).unwrap();
        out.into_inner()
    }

    fn two_by_one() -> RgbaImage {
        RgbaImage::from_raw(2, 1, vec![255, 0, 0, 255, 0, 0, 255, 128]).unwrap()
    }

    #[test]
    fn decodes_png_with_its_alpha() {
        let (w, h, px) = run(&encoded(&two_by_one(), image::ImageFormat::Png)).unwrap();
        assert_eq!((w, h), (2, 1));
        assert_eq!(px, [255, 0, 0, 255, 0, 0, 255, 128]);
    }

    #[test]
    fn decodes_the_other_common_formats() {
        for format in [image::ImageFormat::Gif, image::ImageFormat::WebP, image::ImageFormat::Bmp, image::ImageFormat::Tiff, image::ImageFormat::Ico] {
            let (w, h, _) = run(&encoded(&two_by_one(), format)).unwrap_or_else(|e| panic!("{format:?}: {e:?}"));
            assert_eq!((w, h), (2, 1), "{format:?}");
        }
    }

    fn gif(frames: &[(u8, u32)]) -> Vec<u8> {
        use image::codecs::gif::{GifEncoder, Repeat};
        let mut out = Vec::new();
        {
            let mut encoder = GifEncoder::new(&mut out);
            encoder.set_repeat(Repeat::Infinite).unwrap();
            for &(shade, ms) in frames {
                let buffer = RgbaImage::from_pixel(3, 2, image::Rgba([shade, 0, 0, 255]));
                encoder.encode_frame(image::Frame::from_parts(buffer, 0, 0, image::Delay::from_numer_denom_ms(ms, 1))).unwrap();
            }
        }
        out
    }

    #[test]
    fn a_gif_that_moves_comes_out_frame_by_frame_with_its_timing() {
        let mut out = Vec::new();
        decode(Cursor::new(gif(&[(255, 100), (128, 250), (0, 40)])), &mut out).unwrap();
        assert_eq!(&out[..4], ANIMATION_MAGIC);
        let word = |at: usize| u32::from_ne_bytes(out[at..at + 4].try_into().unwrap());
        assert_eq!((word(4), word(8)), (3, 2));
        let frame = 3 * 2 * 4;
        assert_eq!(word(out.len() - 4), 3);
        assert_eq!(out.len(), HEADER + 3 * frame + 3 * 4 + 4);
        let delays = HEADER + 3 * frame;
        assert_eq!([word(delays), word(delays + 4), word(delays + 8)], [100, 250, 40]);
        // Each frame whole, in order.
        assert_eq!([out[HEADER], out[HEADER + frame], out[HEADER + 2 * frame]], [255, 128, 0]);
    }

    #[test]
    fn text_comes_out_as_a_document_and_binary_doesnt() {
        let mut out = Vec::new();
        document(&b"# Hi\n\nthere\n"[..], &mut out, true).unwrap();
        let doc = crate::doc::decode(&out).unwrap();
        assert_eq!((doc.blocks.len(), doc.cut), (2, false));
        let mut out = Vec::new();
        document(&b"# Hi\n"[..], &mut out, false).unwrap();
        assert_eq!(crate::doc::decode(&out).unwrap().blocks[0].kind, crate::doc::Kind::Plain, "as it is, not as Markdown");
        assert_eq!(document(&b"\x7fELF\x02\x01\x01\0\0\0"[..], &mut Vec::new(), false), Err(Failure::Unsupported));
        let mut out = Vec::new();
        document(&b"<div align=\"center\">\n<img src=\"logo.png\">\n</div>\n"[..], &mut out, true).unwrap();
        let doc = crate::doc::decode(&out).unwrap();
        assert!(doc.blocks[0].kind == crate::doc::Kind::Plain && doc.blocks[0].spans[0].text.starts_with("<div"), "all HTML shows as it is");
    }

    #[test]
    fn a_gif_with_one_frame_is_a_still_image() {
        let (w, h, px) = run(&gif(&[(200, 100)])).unwrap();
        assert_eq!((w, h, px[0]), (3, 2, 200));
    }

    /// A JPEG whose EXIF says to turn it a quarter clockwise, the way phones save portraits.
    fn sideways_jpeg() -> Vec<u8> {
        let mut plain = Vec::new();
        let image = RgbImage::from_pixel(4, 2, image::Rgb([200, 10, 10]));
        JpegEncoder::new(&mut plain).write_image(image.as_raw(), 4, 2, image::ExtendedColorType::Rgb8).unwrap();
        let mut exif = b"Exif\0\0".to_vec();
        exif.extend_from_slice(b"MM\0\x2a\0\0\0\x08\0\x01"); // Big-endian TIFF, one tag.
        exif.extend_from_slice(b"\x01\x12\0\x03\0\0\0\x01\0\x06\0\0\0\0\0\0"); // Orientation = 6.
        let mut out = plain[..2].to_vec();
        out.extend_from_slice(&[0xff, 0xe1]);
        out.extend_from_slice(&(exif.len() as u16 + 2).to_be_bytes());
        out.extend_from_slice(&exif);
        out.extend_from_slice(&plain[2..]);
        out
    }

    #[test]
    fn sideways_photos_come_out_upright() {
        let (w, h, _) = run(&sideways_jpeg()).unwrap();
        assert_eq!((w, h), (2, 4));
    }

    #[test]
    fn something_that_isnt_an_image_is_unsupported() {
        assert_eq!(run(b"just some words, nothing to see here").unwrap_err(), Failure::Unsupported);
        assert_eq!(run(b"").unwrap_err(), Failure::Unsupported);
    }

    #[test]
    fn a_damaged_image_says_so() {
        let png = encoded(&two_by_one(), image::ImageFormat::Png);
        assert_eq!(run(&png[..png.len() / 2]).unwrap_err(), Failure::Damaged);
        assert_eq!(run(b"\x89PNG\r\n\x1a\ngarbage").unwrap_err(), Failure::Damaged);
    }

    #[test]
    fn a_huge_claim_is_refused_before_any_memory_is_spent() {
        // A valid PNG saying 60,000 x 60,000, with hardly any data behind it.
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
        let body = [&60_000u32.to_be_bytes()[..], &60_000u32.to_be_bytes()[..], &[8, 6, 0, 0, 0]].concat();
        let mut crc_input = b"IHDR".to_vec();
        crc_input.extend_from_slice(&body);
        png.extend_from_slice(&body);
        png.extend_from_slice(&crc32(&crc_input).to_be_bytes());
        png.extend_from_slice(b"\0\0\0\x01IDAT\0");
        png.extend_from_slice(&crc32(b"IDAT\0").to_be_bytes());
        assert_eq!(run(&png).unwrap_err(), Failure::TooLarge);
    }

    /// Runs one test of this binary in a process of its own, with `GANNET_SANDBOX_TRY` telling
    /// `sandboxed_child` what to attempt once it's locked down. `tests/e2e.py` does the same in
    /// the real decoder.
    fn try_in_sandbox(what: &str) -> std::process::ExitStatus {
        std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "decode::tests::sandboxed_child", "--nocapture", "--test-threads=1"])
            .env("GANNET_SANDBOX_TRY", what)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
    }

    /// Not a test by itself: does nothing unless `try_in_sandbox` started it.
    #[test]
    fn sandboxed_child() {
        let Ok(what) = std::env::var("GANNET_SANDBOX_TRY") else { return };
        let png = encoded(&two_by_one(), image::ImageFormat::Png);
        let jpeg = sideways_jpeg();
        let others: Vec<Vec<u8>> = [image::ImageFormat::Gif, image::ImageFormat::WebP, image::ImageFormat::Bmp, image::ImageFormat::Tiff, image::ImageFormat::Ico].map(|f| encoded(&two_by_one(), f)).into();
        let big = encoded(&RgbaImage::from_raw(4000, 4000, vec![9u8; 4000 * 4000 * 4]).unwrap(), image::ImageFormat::Png);
        let moving = gif(&[(1, 50), (2, 50)]);
        let markdown = "# Title\n\n- *one*\n- [two](x)\n\n```\ncode\n```\n".repeat(4000);
        assert!(sandbox::enter());
        if what == "decode" {
            decode(Cursor::new(&moving), &mut Vec::new()).unwrap();
            for as_markdown in [false, true] {
                let mut out = Vec::new();
                document(markdown.as_bytes(), &mut out, as_markdown).unwrap();
                assert!(crate::doc::decode(&out).is_some_and(|d| d.cut));
            }
            run(&png).unwrap();
            run(&jpeg).unwrap();
            others.iter().for_each(|other| drop(run(other).unwrap()));
            run(&big).unwrap();
            std::process::exit(REACHED_THE_END);
        }
        // Past the harness, which isn't allowed to carry on as normal in here.
        std::process::exit(attempt(&what));
    }

    #[test]
    fn a_locked_down_process_can_still_decode() {
        assert_eq!(try_in_sandbox("nothing").code(), Some(REACHED_THE_END));
        assert_eq!(try_in_sandbox("decode").code(), Some(REACHED_THE_END));
        assert_eq!(try_in_sandbox("fstat-is-fine").code(), Some(REACHED_THE_END));
    }

    #[test]
    fn the_sandbox_stops_everything_a_takeover_would_want() {
        use std::os::unix::process::ExitStatusExt;
        for what in ["open", "stat-path", "statx-path"] {
            assert_eq!(try_in_sandbox(what).code(), Some(REFUSED), "{what}: refused, and the process lives on");
        }
        for what in ["socket", "exec", "fork", "thread", "exec-memory", "make-executable", "signal-another"] {
            assert_eq!(try_in_sandbox(what).signal(), Some(libc::SIGSYS), "{what}");
        }
    }

    #[test]
    fn running_out_of_memory_still_says_so() {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(try_in_sandbox("out-of-memory").signal(), Some(libc::SIGABRT));
    }

    fn crc32(data: &[u8]) -> u32 {
        let mut crc = !0u32;
        for &b in data {
            crc ^= b as u32;
            for _ in 0..8 {
                crc = if crc & 1 == 1 { (crc >> 1) ^ 0xedb8_8320 } else { crc >> 1 };
            }
        }
        !crc
    }
}
