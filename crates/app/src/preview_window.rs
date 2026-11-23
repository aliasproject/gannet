//! The preview window as a process: `gannet --preview FILE`, which the list starts on Space.
//! Running apart means a file that crashes something while it's being shown takes down only
//! the preview, and the list says so.
//!
//! With `--follow`, the two talk over the child's stdin and stdout: each sends a path, ended by a
//! NUL as a name can hold a newline, whenever its cursor moves. The list's paths start with two
//! bytes for its order, so the window walks the folder in the order the list shows it. The list closes the window by
//! closing the child's stdin, and the child takes the end of its stdin to mean the list has gone.

use std::ffi::OsString;
use std::io::{Read, Write};
use std::os::fd::AsFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::process::{ChildStdin, Command, Stdio};

use gannet_core::listing::{SortKey, SortSpec};

use crate::app::{Hub, Msg};

pub const ARG: &str = "--preview";
pub const APP_ID: &str = "com.aliasproject.Gannet.Preview";
/// A path this long or longer can't go down the pipe in one write, so it isn't sent.
const MAX_MESSAGE: usize = 4096;

/// What `gannet --preview` was asked to show, and how.
#[derive(Debug, PartialEq)]
pub struct Args {
    pub path: PathBuf,
    pub hidden: bool,
    /// `None` without `--sort` or `--descending`: the folder's own saved order then.
    pub sort: Option<SortSpec>,
    pub follow: bool,
}

pub const USAGE: &str = "gannet --preview [--hidden] [--sort name|size|modified] [--descending] [--follow] FILE";

/// The arguments after `--preview`. `None` if they don't make sense.
pub fn parse(args: impl IntoIterator<Item = OsString>) -> Option<Args> {
    let mut out = Args { path: PathBuf::new(), hidden: false, sort: None, follow: false };
    let mut args = args.into_iter();
    let mut path = None;
    while let Some(a) = args.next() {
        match a.to_str() {
            Some("--hidden") => out.hidden = true,
            Some("--follow") => out.follow = true,
            Some("--descending") => out.sort.get_or_insert_with(SortSpec::default).descending = true,
            Some("--sort") => {
                out.sort.get_or_insert_with(SortSpec::default).key = match args.next()?.to_str()? {
                    "name" => SortKey::Name,
                    "size" => SortKey::Size,
                    "modified" => SortKey::Modified,
                    _ => return None,
                }
            }
            Some("--") => {
                path = Some(args.next()?);
                break;
            }
            Some(s) if s.starts_with("--") => return None,
            _ => path = Some(a),
        }
        if path.is_some() {
            break;
        }
    }
    if args.next().is_some() {
        return None;
    }
    out.path = path.map(PathBuf::from)?;
    Some(out)
}

fn command_line(path: &Path, hidden: bool, sort: SortSpec) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![ARG.into(), "--follow".into()];
    if hidden {
        args.push("--hidden".into());
    }
    let key = match sort.key {
        SortKey::Name => "name",
        SortKey::Size => "size",
        SortKey::Modified => "modified",
    };
    args.extend(["--sort".into(), key.into()]);
    if sort.descending {
        args.push("--descending".into());
    }
    args.extend(["--".into(), path.as_os_str().to_owned()]);
    args
}

/// The list's handle on its preview window. Dropping it closes the window.
pub struct Child {
    stdin: ChildStdin,
}

impl Child {
    /// Starts the window. Its answers, and its ending, come back through `hub`.
    pub fn start(hub: &Hub, generation: u64, path: &Path, hidden: bool, sort: SortSpec) -> std::io::Result<Self> {
        // `/proc/self/exe` rather than a path: this binary, even if it's been upgraded since.
        let mut child = Command::new("/proc/self/exe").args(command_line(path, hidden, sort)).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn()?;
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(std::io::Error::other("no pipes"));
        };
        // A window that stops reading mustn't be able to stop the list.
        rustix::fs::fcntl_setfl(stdin.as_fd(), rustix::fs::OFlags::NONBLOCK)?;
        let hub = hub.clone();
        std::thread::Builder::new().name("gannet-previewer".into()).spawn(move || {
            read_paths(stdout, |path| hub.send(Msg::PreviewStepped { generation, path }));
            // Its stdout has ended, which it only does by exiting.
            let crashed = !child.wait().is_ok_and(|s| s.success());
            hub.send(Msg::PreviewEnded { generation, crashed });
        })?;
        Ok(Self { stdin })
    }

    /// Asks the window to show another file, in the list's order. If the window is too busy to
    /// take it, it's dropped: the next move sends the newest anyway.
    pub fn show(&mut self, path: &Path, sort: SortSpec) {
        let mut message = show_message(path, sort);
        message.push(0);
        // Up to this size, a write to a pipe goes in whole or not at all.
        if message.len() <= MAX_MESSAGE {
            let _ = self.stdin.write(&message);
        }
    }
}

/// In the preview window: tells the list which file is showing.
pub fn tell_the_list(path: &Path) {
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(path.as_os_str().as_bytes());
    let _ = out.write_all(&[0]);
    let _ = out.flush();
}

/// In the preview window: listens to the list until it goes.
pub fn listen_to_the_list(hub: Hub) {
    let them = hub.clone();
    let started = std::thread::Builder::new().name("gannet-list".into()).spawn(move || {
        read_paths(std::io::stdin().lock(), |message| {
            if let Some((path, sort)) = read_show(message) {
                them.send(Msg::PreviewShow(path, sort));
            }
        });
        them.send(Msg::ListGone);
    });
    if started.is_err() {
        hub.send(Msg::ListGone);
    }
}

/// What the list sends: the order's key and direction, a byte each, then the path.
fn show_message(path: &Path, sort: SortSpec) -> Vec<u8> {
    let key = match sort.key {
        SortKey::Name => b'n',
        SortKey::Size => b's',
        SortKey::Modified => b'm',
    };
    let mut out = vec![key, if sort.descending { b'-' } else { b'+' }];
    out.extend_from_slice(path.as_os_str().as_bytes());
    out
}

fn read_show(message: PathBuf) -> Option<(PathBuf, SortSpec)> {
    let bytes = message.into_os_string().into_vec();
    let (&[key, dir], path) = bytes.split_first_chunk::<2>()?;
    let key = match key {
        b'n' => SortKey::Name,
        b's' => SortKey::Size,
        b'm' => SortKey::Modified,
        _ => return None,
    };
    let descending = match dir {
        b'+' => false,
        b'-' => true,
        _ => return None,
    };
    if path.is_empty() {
        return None;
    }
    Some((PathBuf::from(OsString::from_vec(path.to_vec())), SortSpec { key, descending }))
}

/// Reads NUL-ended paths until the end, passing each on. A path that's too long to be one is
/// dropped.
fn read_paths(mut from: impl Read, mut each: impl FnMut(PathBuf)) {
    let mut pending = Vec::new();
    // Partway through a message too long to keep, which ends at the next NUL.
    let mut skipping = false;
    let mut buf = [0u8; 8192];
    loop {
        let n = match from.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return,
        };
        pending.extend_from_slice(&buf[..n]);
        while let Some(end) = pending.iter().position(|&b| b == 0) {
            let message: Vec<u8> = pending.drain(..=end).take(end).collect();
            if !std::mem::take(&mut skipping) && !message.is_empty() && message.len() < MAX_MESSAGE {
                each(PathBuf::from(OsString::from_vec(message)));
            }
        }
        if pending.len() >= MAX_MESSAGE {
            pending.clear();
            skipping = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Option<Args> {
        parse(list.iter().map(OsString::from))
    }

    #[test]
    fn arguments_round_trip() {
        let sort = SortSpec { key: SortKey::Modified, descending: true };
        let line = command_line(Path::new("/a/--odd name"), true, sort);
        assert_eq!(line[0], ARG);
        let back = parse(line.into_iter().skip(1)).unwrap();
        assert_eq!(back, Args { path: "/a/--odd name".into(), hidden: true, sort: Some(sort), follow: true });
    }

    #[test]
    fn without_an_order_the_folder_keeps_its_own() {
        assert_eq!(args(&["x.png"]).unwrap().sort, None);
        assert_eq!(args(&["--descending", "x.png"]).unwrap().sort, Some(SortSpec { key: SortKey::Name, descending: true }));
    }

    #[test]
    fn the_lists_order_comes_with_each_path() {
        for sort in [SortSpec::default(), SortSpec { key: SortKey::Modified, descending: true }, SortSpec { key: SortKey::Size, descending: false }] {
            let path = PathBuf::from(OsString::from_vec(b"/a/new\nline \xff".to_vec()));
            let message = PathBuf::from(OsString::from_vec(show_message(&path, sort)));
            assert_eq!(read_show(message), Some((path, sort)));
        }
        assert_eq!(read_show(PathBuf::from("x")), None);
        assert_eq!(read_show(PathBuf::from("n+")), None);
        assert_eq!(read_show(PathBuf::from("q+/a")), None);
    }

    #[test]
    fn plain_and_odd_arguments() {
        assert_eq!(args(&["x.png"]).unwrap().path, PathBuf::from("x.png"));
        assert!(!args(&["x.png"]).unwrap().follow);
        assert_eq!(args(&["--", "--hidden"]).unwrap().path, PathBuf::from("--hidden"));
        assert!(args(&[]).is_none());
        assert!(args(&["--sort", "colour", "x"]).is_none());
        assert!(args(&["--what", "x"]).is_none());
        assert!(args(&["a", "b"]).is_none(), "one file");
    }

    #[test]
    fn paths_come_through_whole_however_they_arrive() {
        // Odd bytes, a newline in a name, and messages split across reads.
        struct Dribble(Vec<u8>, usize);
        impl Read for Dribble {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                let n = 3.min(self.0.len() - self.1).min(buf.len());
                buf[..n].copy_from_slice(&self.0[self.1..self.1 + n]);
                self.1 += n;
                Ok(n)
            }
        }
        let mut stream = b"/a/b\0/c/new\nline\0\0/d/\xff\0".to_vec();
        stream.extend(std::iter::repeat_n(b'x', MAX_MESSAGE + 10));
        stream.extend_from_slice(b"\0/e\0");
        let mut got = Vec::new();
        read_paths(Dribble(stream, 0), |p| got.push(p.into_os_string().into_vec()));
        assert_eq!(got, [b"/a/b".to_vec(), b"/c/new\nline".to_vec(), b"/d/\xff".to_vec(), b"/e".to_vec()]);
    }
}
