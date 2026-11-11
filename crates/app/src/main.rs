//! Gannet: a very fast file manager for Wayland.

mod app;
mod canvas;
mod decode;
mod doc;
mod docview;
mod field;
mod format;
mod icons;
mod media;
mod paint;
mod preview;
mod preview_window;
mod service;
mod text;
mod wl;

use std::ffi::OsString;
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::channel;
use std::time::Instant;

use app::{App, Cmd, Hub, Msg};
use gannet_core::clipboard;
use gannet_core::mounts::read_mounts;
use gannet_core::ops::Engine;
use gannet_core::places::{disk_places, standard_places};
use gannet_core::theme::{self, ThemePaths, ThemeWatch};
use gannet_core::trash::TrashConfig;
use gannet_core::uri::from_file_uri;
use gannet_core::watch::Watch;
use rustix::event::{EventfdFlags, PollFd, PollFlags, Timespec, eventfd, poll};
use wayland_client::backend::WaylandError;

struct Dirs {
    home: PathBuf,
    config: PathBuf,
    cache: PathBuf,
    data: PathBuf,
    /// The shared-mime-info folders, first wins.
    mime: Vec<PathBuf>,
}

/// The one place Gannet reads where things live. Everything else gets paths as arguments.
fn dirs() -> Dirs {
    let home = std::env::var_os("HOME").map(PathBuf::from).filter(|p| p.is_absolute()).unwrap_or_else(|| "/".into());
    let var = |name: &str, default: &str| std::env::var_os(name).map(PathBuf::from).filter(|p| p.is_absolute()).unwrap_or_else(|| home.join(default));
    let data = var("XDG_DATA_HOME", ".local/share");
    let data_dirs = std::env::var_os("XDG_DATA_DIRS").filter(|d| !d.is_empty()).unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    let mut mime = vec![data.join("mime")];
    mime.extend(std::env::split_paths(&data_dirs).filter(|p| p.is_absolute()).map(|p| p.join("mime")));
    Dirs { config: var("XDG_CONFIG_HOME", ".config"), cache: var("XDG_CACHE_HOME", ".cache"), data, home, mime }
}

/// The language messages are in, as GLib chooses it for GTK apps: the first of `LANGUAGE`, then
/// the locale.
fn language() -> Option<String> {
    language_from(|v| std::env::var(v).ok())
}

fn language_from(var: impl Fn(&str) -> Option<String>) -> Option<String> {
    let preferred = var("LANGUAGE").and_then(|l| l.split(':').find(|l| !l.is_empty()).map(str::to_string));
    preferred.or_else(|| ["LC_ALL", "LC_MESSAGES", "LANG"].iter().find_map(|v| var(v).filter(|l| !l.is_empty())))
}

/// What to show: the folder, and an entry to put the cursor on if a file was named.
fn start_location(arg: Option<OsString>, home: &Path) -> PathBuf {
    let Some(arg) = arg else { return home.to_path_buf() };
    let path = arg.to_str().and_then(from_file_uri).unwrap_or_else(|| PathBuf::from(arg));
    let abs = if path.is_absolute() { path } else { std::env::current_dir().unwrap_or_else(|_| "/".into()).join(path) };
    app::clean(&abs)
}

fn main() {
    // Folder names aren't always valid UTF-8, so neither are arguments.
    let arg = std::env::args_os().nth(1);
    if let Some(a) = arg.as_deref().and_then(|a| a.to_str()) {
        if a == decode::ARG {
            decode::helper_main(std::env::args().nth(2).as_deref());
        }
        if a == media::ARG {
            media::helper_main(&std::env::args().skip(2).collect::<Vec<_>>());
        }
        #[cfg(feature = "bench")]
        {
            let rest: Vec<String> = std::env::args().skip(2).collect();
            match a {
                media::TRY_ARG => media::try_main(rest.first().map_or("nothing", |w| w)),
                media::BENCH_TRY_ARG => media::bench_try(rest.first().map_or("nothing", |w| w)),
                media::BENCH_RUN_ARG => media::bench_run(&rest[0], &rest[1..]),
                _ => {}
            }
        }
        #[cfg(feature = "bench")]
        if a == decode::TRY_ARG {
            decode::try_main(std::env::args().nth(2).as_deref().unwrap_or("nothing"));
        }
        if a == service::ARG {
            service::main();
        }
        if a == service::CLAIM_ARG || a == service::RELEASE_ARG {
            service::claim_main(a == service::RELEASE_ARG);
        }
        if a == "--help" || a == "-h" {
            println!("usage: gannet [FOLDER or FILE]\n       gannet --select PATH...\n       {}\n       gannet {} | {}\n       gannet {}", preview_window::USAGE, service::CLAIM_ARG, service::RELEASE_ARG, service::ARG);
            return;
        }
        if a == "--version" {
            println!("gannet {}", env!("CARGO_PKG_VERSION"));
            return;
        }
    }
    let d = dirs();
    // `--preview FILE`: the preview window, on its own.
    let viewing = match arg.as_deref().is_some_and(|a| a == preview_window::ARG) {
        false => None,
        true => match preview_window::parse(std::env::args_os().skip(2)) {
            Some(mut v) => {
                let abs = start_location(Some(v.path.into_os_string()), &d.home);
                v.path = abs;
                Some(v)
            }
            None => {
                eprintln!("usage: {}", preview_window::USAGE);
                std::process::exit(2);
            }
        },
    };
    // `--select PATH...`: the first path's folder, with the paths in it selected.
    let selecting = arg.as_deref().is_some_and(|a| a == service::SELECT_ARG);
    let picks: Vec<PathBuf> = if selecting { std::env::args_os().skip(2).map(|a| start_location(Some(a), &d.home)).collect() } else { Vec::new() };
    let start_dir = match (picks.first(), &viewing) {
        (_, Some(v)) => v.path.parent().map_or_else(|| v.path.clone(), Path::to_path_buf),
        (Some(first), _) => first.parent().map_or_else(|| first.clone(), Path::to_path_buf),
        (None, _) if selecting => d.home.clone(),
        (None, _) => start_location(arg, &d.home),
    };
    let theme_paths = ThemePaths::for_home(&d.home, &d.config);
    let (colors, source) = theme::load(&theme_paths);

    let window = match &viewing {
        Some(_) => wl::Wl::connect("Preview", preview_window::APP_ID, 960, 720),
        None => wl::Wl::connect("Gannet", "com.aliasproject.Gannet", 1100, 700),
    };
    let mut wl = match window {
        Ok(w) => w,
        Err(e) => {
            eprintln!("gannet: {e}");
            std::process::exit(1);
        }
    };

    let mut text = text::Text::new(14.0, true);
    text.set_contrast(colors.dark);
    let primary = text::primary_font(&d.cache);
    match &primary {
        Some((path, index)) => {
            text.add_font(path, *index);
        }
        None => eprintln!("gannet: no font found; is fontconfig installed?"),
    }

    let wake = Arc::new(eventfd(0, EventfdFlags::CLOEXEC | EventfdFlags::NONBLOCK).expect("eventfd"));
    let (tx, rx) = channel();
    let hub = Hub { tx, wake: wake.clone() };

    let mut places = standard_places(&d.home, &d.config, &d.data);
    places.extend(disk_places(&read_mounts()));

    let (w, h) = wl.physical_size();
    let op_hub = hub.clone();
    let trash = TrashConfig::new(&d.data, rustix::process::geteuid().as_raw());
    let engine = Engine::new(trash, Arc::new(move |e| op_hub.send(Msg::Op(e))));
    let mut app = App::new(hub.clone(), engine, d.data.join("Trash/files"), colors, source, text, places, start_dir.clone(), w, h, wl.scale());
    let dir_watch = Watch::new().ok().map(Arc::new);
    app.watch = dir_watch.clone();
    app.cache_home = Some(d.cache.clone());
    app.kinds = Arc::new(preview::Kinds::new(d.mime.clone(), language()));
    let mut theme_watch = ThemeWatch::new(theme_paths.clone()).ok();
    let mut theme_due: Option<Instant> = None;
    #[cfg(feature = "bench")]
    {
        app.slow_read = std::env::var("GANNET_SLOW_READ_MS").ok().and_then(|v| v.parse().ok()).map(std::time::Duration::from_millis);
        app.slow_preview = std::env::var("GANNET_SLOW_PREVIEW_MS").ok().and_then(|v| v.parse().ok()).map(std::time::Duration::from_millis);
    }
    let names: Vec<Vec<u8>> = picks.iter().filter(|p| p.parent() == Some(start_dir.as_path())).filter_map(|p| p.file_name()).map(|n| n.as_encoded_bytes().to_vec()).collect();
    // The list's preview window, while one is open.
    let mut previewer: Option<preview_window::Child> = None;
    let following = viewing.as_ref().is_some_and(|v| v.follow);
    if let Some(v) = viewing {
        app.show_hidden = v.hidden;
        app.sort = v.sort;
        app.start_viewer(v.path);
        if v.follow {
            preview_window::listen_to_the_list(hub.clone());
        }
    } else if names.is_empty() {
        app.go(gannet_core::history::Stop { dir: start_dir, cursor_name: None });
    } else {
        app.open_selecting(start_dir, names);
    }
    let mut first_frame = true;
    #[cfg(feature = "bench")]
    let mut bench = bench::Bench::new(app.viewer);

    loop {
        #[cfg(feature = "bench")]
        let busy_start = Instant::now();
        if let Err(e) = wl.queue.dispatch_pending(&mut wl.state) {
            eprintln!("gannet: Wayland error: {e}");
            say_why_the_connection_ended(&wl);
            std::process::exit(1);
        }
        while let Some(i) = wl.state.inputs.pop_front() {
            if matches!(i, wl::Input::Resized) {
                let (w, h) = wl.physical_size();
                app.resized(w, h, wl.scale());
            }
            app.input(i);
        }
        while let Ok(m) = rx.try_recv() {
            app.on_msg(m);
        }
        let now = Instant::now();
        app.on_timer(now);
        if theme_due.is_some_and(|t| t <= now) {
            theme_due = None;
            if let Some(c) = theme::reload(&theme_paths) {
                app.set_colors(c, theme::Source::AliasOs);
            }
        }
        for cmd in app.cmds.drain(..) {
            match cmd {
                Cmd::Title(t) => wl.set_title(&t),
                Cmd::Cursor(c) => wl.set_cursor(c),
                Cmd::SetClipboard(offers) => {
                    wl.set_clipboard(wl::Offer { mimes: offers.into_iter().map(|(m, d)| (m.to_string(), Arc::new(d))).collect() });
                }
                Cmd::ClearClipboard => wl.clear_clipboard(),
                Cmd::ReadClipboard(purpose) => read_clipboard(&wl, &hub, purpose),
                Cmd::StartDrag { formats, marker, move_only, icon } => {
                    let mut mimes: Vec<_> = formats.into_iter().map(|(m, d)| (m.to_string(), Arc::new(d))).collect();
                    mimes.extend(marker.map(|m| (m.mime(), Arc::new(Vec::new()))));
                    wl.start_drag(wl::Offer { mimes }, move_only, icon);
                }
                Cmd::CancelDrag => wl.cancel_drag(),
                Cmd::AcceptDrop(accept) => wl.accept_drop(accept),
                Cmd::ReadDrop { dest, trash, decided } => read_drop(&wl, &hub, dest, trash, decided),
                Cmd::FinishDrop => wl.finish_drop(),
                Cmd::AbandonDrop => wl.abandon_drop(),
                Cmd::OpenPreview { generation, path, hidden, sort } => {
                    // Replacing one closes it.
                    previewer = None;
                    match preview_window::Child::start(&hub, generation, &path, hidden, sort) {
                        Ok(child) => previewer = Some(child),
                        Err(e) => {
                            hub.send(Msg::Notice(format!("Couldn't open the preview: {e}")));
                            hub.send(Msg::PreviewEnded { generation, crashed: false });
                        }
                    }
                }
                Cmd::ClosePreview => previewer = None,
                Cmd::PreviewShow(path) => {
                    if let Some(p) = previewer.as_mut() {
                        p.show(&path);
                    }
                }
                Cmd::PreviewStepped(path) => {
                    if following {
                        preview_window::tell_the_list(&path);
                    }
                }
                Cmd::Quit => return,
            }
        }
        if wl.must_paint() || (!app.dirty.is_empty() && wl.can_paint() && app.hold_paint.is_none()) {
            let damage = if wl.must_paint() { canvas::Rect::new(0, 0, i32::MAX / 2, i32::MAX / 2) } else { std::mem::take(&mut app.dirty) };
            app.dirty = canvas::Rect::default();
            #[cfg(feature = "bench")]
            let t0 = Instant::now();
            wl.paint(damage, |c| app.paint(c));
            #[cfg(feature = "bench")]
            bench.painted(t0.elapsed(), &app);
            if first_frame {
                first_frame = false;
                after_first_frame(&hub, &d.cache, primary.clone());
                app.ask_desktop_theme();
            }
            let mut missing = app.text.take_missing();
            missing.extend(app.doc_fonts.as_mut().map(|f| f.take_missing()).unwrap_or_default());
            if !missing.is_empty() {
                find_fallback_fonts(&hub, missing);
            }
        }
        #[cfg(feature = "bench")]
        bench.idle(&app);
        #[cfg(feature = "bench")]
        bench.busy(busy_start.elapsed());
        // A full socket isn't an error: wait until it can take more. Anything else means
        // the compositor has gone, and a window with no compositor has nothing to do.
        let flush_blocked = match wl.conn.flush() {
            Ok(()) => false,
            Err(WaylandError::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => true,
            Err(e) => {
                eprintln!("gannet: lost the Wayland connection: {e}");
                say_why_the_connection_ended(&wl);
                std::process::exit(1);
            }
        };

        // Sleep until something happens. With nothing due, this is an endless wait.
        let Some(guard) = wl.queue.prepare_read() else { continue };
        let deadline = [app.next_deadline(), theme_due].into_iter().flatten().min();
        let timeout = deadline.map(|t| {
            let d = t.saturating_duration_since(Instant::now());
            Timespec { tv_sec: d.as_secs() as _, tv_nsec: d.subsec_nanos() as _ }
        });
        let mut fds = vec![PollFd::from_borrowed_fd(wl.fd(), if flush_blocked { PollFlags::IN | PollFlags::OUT } else { PollFlags::IN }), PollFd::new(&*wake, PollFlags::IN)];
        let repeat = wl.repeat_fd();
        let repeat_at = repeat.map(|f| {
            fds.push(PollFd::from_borrowed_fd(f, PollFlags::IN));
            fds.len() - 1
        });
        let dir_at = dir_watch.as_ref().map(|w| {
            fds.push(PollFd::from_borrowed_fd(w.as_ref().as_fd(), PollFlags::IN));
            fds.len() - 1
        });
        let theme_at = theme_watch.as_ref().map(|w| {
            fds.push(PollFd::from_borrowed_fd(w.watch.as_fd(), PollFlags::IN));
            fds.len() - 1
        });
        let _ = poll(&mut fds, timeout.as_ref());
        let ready = |i: usize| fds[i].revents().intersects(PollFlags::IN | PollFlags::ERR | PollFlags::HUP);
        let (wl_ready, wake_ready) = (ready(0), ready(1));
        let wl_hup = fds[0].revents().intersects(PollFlags::HUP | PollFlags::ERR);
        let repeat_ready = repeat_at.is_some_and(ready);
        let dir_ready = dir_at.is_some_and(ready);
        let theme_ready = theme_at.is_some_and(ready);
        drop(fds);
        let read_error = if wl_ready { guard.read().err().filter(read_is_fatal) } else { None };
        if read_error.is_some() || wl_hup {
            // The compositor went away, or it disconnected this program, in which case it may have
            // said why. A hang-up with nothing wrong is just the session ending.
            let (w, h) = wl.physical_size();
            match &read_error {
                Some(e) => eprintln!("gannet: reading from the compositor failed: {e} (window {w}x{h})"),
                None => eprintln!("gannet: the compositor hung up (window {w}x{h})"),
            }
            if say_why_the_connection_ended(&wl) || read_error.is_some() {
                std::process::exit(1);
            }
            return;
        }
        if wake_ready {
            let mut buf = [0u8; 8];
            let _ = rustix::io::read(&*wake, &mut buf);
        }
        if repeat_ready {
            wl.on_repeat_timer();
        }
        if dir_ready && dir_watch.as_ref().is_some_and(|w| w.drain()) {
            app.folder_changed();
        }
        if theme_ready && theme_watch.as_mut().is_some_and(ThemeWatch::changed) {
            // A swap deletes then moves; a short wait lets it finish.
            theme_due = Some(Instant::now() + std::time::Duration::from_millis(15));
        }
    }
}

/// A read reports `WouldBlock` when it had nothing to dispatch, which is ordinary: all that arrived
/// might be a message the library handles itself, like the `delete_id` from destroying a buffer,
/// and resizing destroys plenty.
fn read_is_fatal(error: &WaylandError) -> bool {
    !matches!(error, WaylandError::Io(e) if e.kind() == std::io::ErrorKind::WouldBlock)
}

/// A compositor that disconnects a client for breaking the protocol leaves the reason here. Returns
/// whether it did.
fn say_why_the_connection_ended(wl: &wl::Wl) -> bool {
    let Some(error) = wl.conn.protocol_error() else { return false };
    let (w, h) = wl.physical_size();
    eprintln!("gannet: the compositor closed the connection: {}@{} error {}: {} (window {w}x{h})", error.object_interface, error.object_id, error.code, error.message);
    true
}

/// Asks for the clipboard's contents. The read happens off to the side, because another
/// program is on the other end of that pipe and might stall.
fn read_clipboard(wl: &wl::Wl, hub: &Hub, purpose: app::Paste) {
    const MAX: u64 = 64 * 1024 * 1024;
    let offered = wl.clipboard_mimes();
    let mime = match purpose {
        app::Paste::Files { .. } => clipboard::preferred_file_mime(&offered),
        app::Paste::IntoField { .. } => clipboard::preferred_text_mime(&offered),
    };
    let fd = mime.and_then(|m| wl.read_clipboard(m));
    let (Some(mime), Some(fd)) = (mime, fd) else {
        // Still an answer: the app is waiting for one.
        hub.send(Msg::Clipboard(Some(Vec::new()), String::new(), purpose));
        return;
    };
    // KDE programs flag a cut in a format of its own.
    let kde = (mime == clipboard::URI_LIST && offered.iter().any(|o| o == clipboard::KDE_CUT)).then(|| wl.read_clipboard(clipboard::KDE_CUT)).flatten();
    let hub = hub.clone();
    let _ = std::thread::Builder::new().name("gannet-paste".into()).spawn(move || {
        let data = clipboard::read_limited(std::fs::File::from(fd), MAX);
        let is_cut = kde.and_then(|k| clipboard::read_limited(std::fs::File::from(k), 16)).is_some_and(|d| d.first() == Some(&b'1'));
        let mime = if is_cut { clipboard::URI_LIST_CUT } else { mime };
        hub.send(Msg::Clipboard(data, mime.to_string(), purpose));
    });
}

/// Reads a dropped file list off the main thread, since the other program can stall.
fn read_drop(wl: &wl::Wl, hub: &Hub, dest: PathBuf, trash: bool, decided: Option<app::DropAction>) {
    const MAX: u64 = 64 * 1024 * 1024;
    let Some((fd, can_copy, can_move)) = wl.read_drop() else {
        hub.send(Msg::Dropped(app::Dropped { paths: Vec::new(), dest, trash, can_copy: false, can_move: false, decided, all_local: false }));
        return;
    };
    // The other program can stall forever, and an unanswered drop would refuse every later one.
    const PATIENCE: std::time::Duration = std::time::Duration::from_secs(10);
    let (answer, answered) = std::sync::mpsc::channel();
    // If this can't start, the sender goes with it, and the waiter below answers right away.
    let _ = std::thread::Builder::new().name("gannet-drop".into()).spawn(move || {
        let _ = answer.send(clipboard::read_limited(std::fs::File::from(fd), MAX));
    });
    let waiter = hub.clone();
    let (wait_dest, wait_trash) = (dest.clone(), trash);
    let waiting = std::thread::Builder::new().name("gannet-drop-wait".into()).spawn(move || {
        let data = answered.recv_timeout(PATIENCE).ok().flatten().unwrap_or_default();
        let (paths, _) = clipboard::parse(clipboard::URI_LIST, &data);
        let all_local = clipboard::uri_list_is_all_local(&data, &paths);
        waiter.send(Msg::Dropped(app::Dropped { paths, dest: wait_dest, trash: wait_trash, can_copy, can_move, decided, all_local }));
    });
    if waiting.is_err() {
        hub.send(Msg::Dropped(app::Dropped { paths: Vec::new(), dest, trash, can_copy: false, can_move: false, decided, all_local: false }));
    }
}

/// Work that mustn't delay the first frame: confirm the font the cache file named.
fn after_first_frame(hub: &Hub, cache: &Path, primary: Option<(PathBuf, u32)>) {
    let (hub, cache) = (hub.clone(), cache.to_path_buf());
    let _ = std::thread::Builder::new().name("gannet-fonts".into()).spawn(move || {
        if let Some(found) = text::fc_match("sans")
            && Some(&found) != primary.as_ref()
        {
            text::store_primary_font(&cache, &found);
            hub.send(Msg::PrimaryFont(found.0, found.1));
        }
    });
}

/// Lookups for one batch of missing characters. Characters left over show blank.
const MAX_FONT_LOOKUPS: usize = 64;

fn find_fallback_fonts(hub: &Hub, chars: Vec<char>) {
    let hub = hub.clone();
    let _ = std::thread::Builder::new().name("gannet-fonts".into()).spawn(move || {
        let mut found: Vec<PathBuf> = Vec::new();
        // A long document can need thousands of characters, and one font found for the first
        // usually has the rest, so it's asked before fontconfig is.
        let mut have: Vec<(Vec<u8>, u32)> = Vec::new();
        // fontconfig always answers with something, even for a character no font has, so a file
        // full of those would still be a process each without a limit.
        let mut asked = 0;
        for ch in chars {
            if asked >= MAX_FONT_LOOKUPS {
                break;
            }
            if have.iter().any(|(bytes, index)| text::font_has(bytes, *index, ch)) {
                continue;
            }
            asked += 1;
            if let Some((path, index)) = text::fc_match(&format!("sans:charset={:x}", ch as u32))
                && !found.contains(&path)
            {
                found.push(path.clone());
                if let Ok(bytes) = std::fs::read(&path) {
                    have.push((bytes, index));
                }
                hub.send(Msg::Font(path, index));
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Error, ErrorKind};

    #[test]
    fn nothing_to_read_isnt_a_lost_connection() {
        assert!(!read_is_fatal(&WaylandError::Io(Error::from(ErrorKind::WouldBlock))));
    }

    #[test]
    fn the_language_is_glibs() {
        let env = |pairs: &'static [(&'static str, &'static str)]| move |v: &str| pairs.iter().find(|(k, _)| *k == v).map(|(_, x)| x.to_string());
        assert_eq!(language_from(env(&[("LANG", "en_US.UTF-8"), ("LANGUAGE", "de:fr")])).as_deref(), Some("de"));
        assert_eq!(language_from(env(&[("LANG", "en_US.UTF-8")])).as_deref(), Some("en_US.UTF-8"));
        assert_eq!(language_from(env(&[("LANG", "en_US.UTF-8"), ("LC_ALL", "pt_BR.UTF-8")])).as_deref(), Some("pt_BR.UTF-8"));
        assert_eq!(language_from(env(&[("LANG", "C.UTF-8"), ("LANGUAGE", ":de")])).as_deref(), Some("de"), "as GLib reads it, in any locale");
        assert_eq!(language_from(env(&[])), None);
    }

    #[test]
    fn a_really_broken_connection_is() {
        assert!(read_is_fatal(&WaylandError::Io(Error::from(ErrorKind::BrokenPipe))));
        assert!(read_is_fatal(&WaylandError::Io(Error::from(ErrorKind::ConnectionReset))));
    }
}

#[cfg(feature = "bench")]
mod bench {
    //! Stamps and state dumps for bench/ and the end-to-end checks.

    use std::io::Write;
    use std::time::Duration;

    use crate::app::{App, Edit};

    pub struct Bench {
        log: Option<std::fs::File>,
        state_path: Option<String>,
        last_state: String,
        /// The folder whose rows have been reported. A fast load can finish between two
        /// paints, so a paint that isn't ready can't be relied on to mark a new one.
        rows_stamped: Option<std::path::PathBuf>,
    }

    pub fn mono_ms() -> f64 {
        let t = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
        t.tv_sec as f64 * 1000.0 + t.tv_nsec as f64 / 1e6
    }

    impl Bench {
        /// The preview window writes beside the list, as both see the same environment.
        pub fn new(viewer: bool) -> Self {
            let suffix = if viewer { ".preview" } else { "" };
            Self {
                log: std::env::var("GANNET_BENCH_LOG").ok().and_then(|p| std::fs::File::create(format!("{p}{suffix}")).ok()),
                state_path: std::env::var("GANNET_STATE").ok().map(|p| format!("{p}{suffix}")),
                last_state: String::new(),
                rows_stamped: None,
            }
        }

        fn stamp(&mut self, line: String) {
            if let Some(f) = &mut self.log {
                let _ = writeln!(f, "{line}");
            }
        }

        pub fn painted(&mut self, took: Duration, app: &App) {
            let ready = app.listing.as_ref().is_some_and(|l| l.complete) && !app.loading && !app.rows.is_empty();
            self.stamp(format!("paint {} {:.1}", took.as_micros(), mono_ms()));
            if ready && self.rows_stamped.as_ref() != Some(&app.dir) {
                self.rows_stamped = Some(app.dir.clone());
                self.stamp(format!("rows {} {:.1} {:.1}", app.rows.len(), mono_ms(), app.load_elapsed().as_secs_f64() * 1000.0));
            }
            if !ready {
                self.rows_stamped = None;
            }
        }

        /// The time spent on one pass of the main loop, which is how long the UI can't respond.
        pub fn busy(&mut self, took: Duration) {
            if took > Duration::from_micros(500) {
                self.stamp(format!("busy {}", took.as_micros()));
            }
        }

        /// Writes the visible state when it changes, for checks to wait on.
        pub fn idle(&mut self, app: &App) {
            let Some(path) = self.state_path.clone() else { return };
            let state = dump(app);
            if state != self.last_state {
                let tmp = format!("{path}.tmp");
                if std::fs::write(&tmp, &state).is_ok() {
                    let _ = std::fs::rename(&tmp, &path);
                }
                self.last_state = state;
            }
        }
    }

    fn esc(s: &str) -> String {
        let mut o = String::with_capacity(s.len() + 2);
        o.push('"');
        for c in s.chars() {
            match c {
                '"' => o.push_str("\\\""),
                '\\' => o.push_str("\\\\"),
                c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
                c => o.push(c),
            }
        }
        o.push('"');
        o
    }

    fn list(v: impl Iterator<Item = String>) -> String {
        format!("[{}]", v.map(|s| esc(&s)).collect::<Vec<_>>().join(","))
    }

    pub fn dump(a: &App) -> String {
        let name = |i: u32| a.listing.as_ref().map_or_else(String::new, |l| String::from_utf8_lossy(l.name(i)).into_owned());
        let edit = match &a.edit {
            Edit::None => "none".to_string(),
            Edit::Path(f) => format!("path:{}", f.text),
            Edit::Filter(f) => format!("filter:{}", f.text),
            Edit::Rename { field, .. } => format!("rename:{}", field.text),
            Edit::NewFolder(f) => format!("newfolder:{}", f.text),
        };
        format!(
            "{{\"dir\":{},\"loading\":{},\"count\":{},\"names\":{},\"cursor\":{},\"cursor_shown\":{},\"selected\":{},\"filter\":{},\"edit\":{},\"hidden\":{},\"sort\":{},\"desc\":{},\"scroll\":{},\"notices\":{},\"menu\":{},\"error\":{},\"base\":{},\"can_back\":{},\"can_forward\":{},\"jobs\":{},\"visible_jobs\":{},\"dialog\":{},\"undo\":{},\"panel\":{},\"preview\":{},\"previewing\":{},\"drag\":{}}}\n",
            esc(&a.dir.to_string_lossy()),
            a.loading,
            a.rows.len(),
            list(a.rows.iter().take(300).map(|&i| name(i))),
            a.cursor,
            a.cursor_shown,
            list(a.selected_ids().into_iter().take(300).map(name)),
            esc(&a.filter),
            esc(&edit),
            a.show_hidden,
            esc(&format!("{:?}", a.sort.key)),
            a.sort.descending,
            a.scroll,
            list(a.notices.iter().cloned()),
            a.menu.is_some(),
            a.load_error.as_ref().map_or_else(|| "null".to_string(), |e| esc(e)),
            a.colors.base,
            a.history.can_go_back(),
            a.history.can_go_forward(),
            a.jobs.len(),
            a.visible_jobs().count(),
            esc(match &a.dialog {
                None => "",
                Some(crate::app::Dialog::Conflict { .. }) => "conflict",
                Some(crate::app::Dialog::ConfirmDelete { empty: true, .. }) => "empty",
                Some(crate::app::Dialog::ConfirmDelete { .. }) => "delete",
                Some(crate::app::Dialog::ConfirmQuit { .. }) => "quit",
            }),
            a.undo.len(),
            a.panel_rows(),
            preview(a),
            a.previewer.as_ref().and_then(|p| p.file_name()).map_or_else(|| "null".to_string(), |n| esc(&n.to_string_lossy())),
            esc(&drag(a)),
        )
    }

    fn drag(a: &App) -> String {
        match &a.file_drag {
            Some(crate::app::FileDrag::Active(d)) if d.is_out() => "out".to_string(),
            Some(crate::app::FileDrag::Active(d)) => a.drag_label(d),
            Some(crate::app::FileDrag::Pressed { .. }) => "pressed".to_string(),
            Some(crate::app::FileDrag::Band(b)) => if b.active { "band" } else { "pressed" }.to_string(),
            None => a.incoming.as_ref().filter(|i| !i.label.is_empty()).and_then(|i| i.target.as_ref().map(|t| a.incoming_label(i, t))).unwrap_or_default(),
        }
    }

    fn preview(a: &App) -> String {
        use crate::preview::{Content, Video};
        let Some(p) = &a.preview else { return "null".to_string() };
        let name = p.path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        let (state, w, h, message) = match &p.content {
            Content::Waiting if p.loading_shown => ("loading", 0, 0, ""),
            Content::Waiting => ("waiting", 0, 0, ""),
            Content::Image { image, thumbnail: true } => ("thumbnail", image.w, image.h, ""),
            Content::Image { image, thumbnail: false } => ("image", image.w, image.h, ""),
            Content::Video(Video { sound: Some(info), .. }) => ("sound", 0, 0, info.description.as_str()),
            Content::Video(video) => ("video", video.frame.map_or(0, |f| f.1), video.frame.map_or(0, |f| f.2), ""),
            Content::Message(m) => ("message", 0, 0, m.as_str()),
            Content::Doc { doc, info } if doc.lines() > 0 => ("text", 0, 0, info.description.as_str()),
            Content::Doc { info, .. } => ("markdown", 0, 0, info.description.as_str()),
            Content::Info(i) => ("info", i.thumbnail.as_ref().map_or(0, |t| t.w), i.thumbnail.as_ref().map_or(0, |t| t.h), i.description.as_str()),
        };
        let kind = match &p.content {
            Content::Info(i) | Content::Video(Video { sound: Some(i), .. }) | Content::Doc { info: i, .. } => esc(&i.mime),
            _ => "null".to_string(),
        };
        let rect = a.preview_rect();
        let video = match &p.content {
            Content::Video(v) => format!(
                "{{\"shown\":{},\"position\":{:.3},\"duration\":{:.3},\"paused\":{},\"volume\":{:.0},\"muted\":{}}}",
                v.frames_shown, v.state.position, v.state.duration, v.state.paused, v.state.volume, v.state.muted
            ),
            _ => "null".to_string(),
        };
        let frames = match &p.content {
            Content::Image { image, .. } => image.frames(),
            _ => 0,
        };
        let doc = match (&p.content, &p.doc_layout) {
            (Content::Doc { doc, .. }, layout) => format!(
                "{{\"blocks\":{},\"cut\":{},\"lines\":{},\"scroll\":{:.0},\"height\":{},\"text\":{}}}",
                doc.blocks.len(),
                doc.cut,
                layout.as_ref().map_or(0, |l| l.lines.len()),
                p.scroll,
                layout.as_ref().map_or(0, |l| l.height),
                esc(&doc.blocks.iter().take(3).flat_map(|b| b.spans.iter().map(|s| s.text.as_str())).collect::<String>().chars().take(200).collect::<String>()),
            ),
            _ => "null".to_string(),
        };
        let folder = match &p.content {
            Content::Info(crate::preview::Info { folder: Some(t), .. }) => format!("{{\"items\":{},\"bytes\":{},\"done\":{},\"incomplete\":{}}}", t.items, t.bytes, t.done, t.incomplete),
            _ => "null".to_string(),
        };
        let zoom = p.zoom.map_or_else(|| "null".to_string(), |z| format!("{z:.3}"));
        format!(
            "{{\"name\":{},\"state\":{},\"kind\":{kind},\"w\":{w},\"h\":{h},\"frame\":{},\"frames\":{frames},\"message\":{},\"zoom\":{zoom},\"pan\":[{:.0},{:.0}],\"scaled\":{},\"size\":{},\"rect\":[{},{},{},{}],\"video\":{video},\"doc\":{doc},\"folder\":{folder}}}",
            esc(&name),
            esc(state),
            p.frame,
            esc(message),
            p.pan.0,
            p.pan.1,
            p.has_scaled(),
            p.size.unwrap_or(0),
            rect.x,
            rect.y,
            rect.w,
            rect.h,
        )
    }
}
