//! `gannet list`, `info`, `trash` and `restore`: the window's file handling for scripts and
//! agents, with no display and no D-Bus.
//!
//! Each prints text for people, or with `--json` one JSON object for programs. The JSON is
//! described in the README and carries a `version`: fields may be added within a version, but
//! none is renamed, removed or given a new meaning without raising it. Errors also go to stderr,
//! one line each, and the exit code is 0 only if everything asked for was done.

use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use crate::folder_sorts::FolderSorts;
use crate::listing::{LoadEvent, LoadRequest, Loaded, SortKey, SortSpec, TrashSpec, spawn_load};
use crate::mounts::Mount;
use crate::ops::{Answer, Choice, Engine, Event, Outcome, Request, Step};
use crate::trash::{self, Info, TrashConfig, Trashed};
use crate::uri::{from_file_uri, to_file_uri};

/// The JSON's version. Raise it only for a change that could break a script reading it.
pub const JSON_VERSION: u32 = 1;

const VERBS: [&str; 4] = ["list", "info", "trash", "restore"];

/// Whether the first argument names one of these commands rather than a folder to open.
pub fn is_verb(arg: &OsStr) -> bool {
    VERBS.iter().any(|v| arg.as_bytes() == v.as_bytes())
}

/// The lines `gannet --help` adds for these commands.
pub const USAGE: &str = "gannet list [--json] [--hidden] [--sort name|size|modified] [--reverse] [FOLDER]\n       gannet info [--json] PATH...\n       gannet trash [--json] PATH...\n       gannet restore [--json] [PATH...]";

/// What the commands need to know about where things are.
pub struct Env {
    /// What relative paths are relative to.
    pub cwd: PathBuf,
    pub trash: TrashConfig,
    /// Where other disks' Trash folders are looked for.
    pub mounts: Vec<Mount>,
    /// The orders the window keeps for each folder, read but never changed here.
    pub sorts: FolderSorts,
    /// How long another disk's Trash gets to answer before the Trash is listed without it.
    pub disks_give_up: Duration,
}

/// Runs `verb` with the arguments after it. Returns the exit code.
pub fn run(env: &mut Env, verb: &str, args: &[OsString], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let opts = match Opts::parse(verb, args) {
        Ok(o) => o,
        Err(message) => {
            // A script that asked for JSON still gets an object to read the error from.
            let json = args.iter().take_while(|a| *a != "--").any(|a| a == "--json");
            let mut report = Report::default();
            report.fail(None, format!("{message}\nusage: {}", usage(verb)));
            return finish(verb, json, Doc { fields: Vec::new(), text: String::new() }, &report, out, err);
        }
    };
    if opts.help {
        let _ = writeln!(out, "{}", help(verb));
        return 0;
    }
    let mut report = Report::default();
    let doc = match verb {
        "list" => list(env, &opts, &mut report),
        "info" => info(env, &opts, &mut report),
        "trash" => trash_cmd(env, &opts, &mut report),
        _ => restore(env, &opts, &mut report),
    };
    finish(verb, opts.json, doc, &report, out, err)
}

/// Prints a command's output and its errors. Returns the exit code.
fn finish(verb: &str, json: bool, doc: Doc, report: &Report, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let text = match json {
        true => {
            let mut fields = vec![("version", J::Int(JSON_VERSION as i64)), ("command", J::str(verb))];
            fields.extend(doc.fields);
            fields.push(("errors", J::Arr(report.errors.iter().map(|(p, m)| J::Obj(vec![("path", p.as_ref().map_or(J::Null, |p| J::path(p))), ("message", J::str(m))])).collect())));
            let mut s = J::Obj(fields).to_string();
            s.push('\n');
            s
        }
        false => doc.text,
    };
    let mut failed = !report.errors.is_empty();
    // A reader that stops early, like `head`, isn't a failure of the command; a full disk is.
    if let Err(e) = out.write_all(text.as_bytes()).and_then(|()| out.flush())
        && e.kind() != std::io::ErrorKind::BrokenPipe
    {
        let _ = writeln!(err, "gannet {verb}: couldn't write the output: {}", crate::listing::describe(&e));
        failed = true;
    }
    for (path, message) in &report.errors {
        let _ = match path {
            Some(p) => writeln!(err, "gannet {verb}: {}: {message}", p.display()),
            None => writeln!(err, "gannet {verb}: {message}"),
        };
    }
    if failed { 1 } else { 0 }
}

fn usage(verb: &str) -> &'static str {
    match verb {
        "list" => "gannet list [--json] [--hidden] [--sort name|size|modified] [--reverse] [FOLDER]",
        "info" => "gannet info [--json] PATH...",
        "trash" => "gannet trash [--json] PATH...",
        _ => "gannet restore [--json] [PATH...]",
    }
}

fn help(verb: &str) -> String {
    let what = match verb {
        "list" => {
            "Lists FOLDER, or the current folder, in the order the window shows it: folders first, then by\n\
             the order last chosen for that folder in the window, or names A to Z.\n\n\
             \x20 --hidden    include names starting with a dot\n\
             \x20 --sort KEY  sort by name (A to Z), size (biggest first) or modified (newest first)\n\
             \x20 --reverse   the other way round"
        }
        "info" => "Says what each PATH is: its kind, size, times and permissions, where a link leads, and for\nan item in the Trash, where it came from and when it was deleted. Links aren't followed.",
        "trash" => "Moves each PATH to the Trash, the one on its own disk if it isn't on the same disk as your home\nfolder's, as the window does. It can be put back with `gannet restore` or from the window.",
        _ => {
            "With no PATH, lists what's in the Trash on every disk, with where each item came from and when\n\
             it was deleted. With PATHs, puts each item back where it came from. A PATH is an item's original\n\
             path, or its path in the Trash; if several items came from one path, the latest one deleted is\n\
             restored. Nothing is ever overwritten: an item whose place is taken stays in the Trash."
        }
    };
    format!("usage: {}\n\n{what}\n\n  --json      print one JSON object (see the README)\n\nThe exit code is 0 on success and 1 if anything failed. Errors go to stderr.", usage(verb))
}

#[derive(Default)]
struct Opts {
    json: bool,
    hidden: bool,
    sort: Option<SortKey>,
    reverse: bool,
    help: bool,
    paths: Vec<OsString>,
}

impl Opts {
    fn parse(verb: &str, args: &[OsString]) -> Result<Opts, String> {
        let mut o = Opts::default();
        let mut it = args.iter();
        let mut flags_done = false;
        while let Some(a) = it.next() {
            let b = a.as_bytes();
            if flags_done || !b.starts_with(b"-") || b == b"-" {
                o.paths.push(a.clone());
                continue;
            }
            let (flag, value) = match a.to_str().and_then(|s| s.split_once('=')) {
                Some((f, v)) => (f.to_string(), Some(v.to_string())),
                None => (a.to_string_lossy().into_owned(), None),
            };
            if value.is_some() && flag != "--sort" {
                return Err(format!("{flag} takes no value"));
            }
            match flag.as_str() {
                "--" => flags_done = true,
                "--help" | "-h" => o.help = true,
                "--json" => o.json = true,
                "--hidden" if verb == "list" => o.hidden = true,
                "--reverse" if verb == "list" => o.reverse = true,
                "--sort" if verb == "list" => {
                    let v = match value {
                        Some(v) => v,
                        None => it.next().filter(|n| !n.as_bytes().starts_with(b"-")).ok_or("--sort needs name, size or modified")?.to_string_lossy().into_owned(),
                    };
                    o.sort = Some(match v.as_str() {
                        "name" => SortKey::Name,
                        "size" => SortKey::Size,
                        "modified" => SortKey::Modified,
                        _ => return Err(format!("--sort takes name, size or modified, not “{v}”")),
                    });
                }
                _ => return Err(format!("unknown option {flag}")),
            }
        }
        if o.help {
            return Ok(o);
        }
        if o.paths.iter().any(|p| p.is_empty()) {
            return Err("an empty PATH names nothing".into());
        }
        // As rm does: trashing `.` or `..` is almost always a mistake in a script.
        // A `file://` URI is checked as it decodes: `%2E` is a dot too.
        let decoded = |p: &OsString| p.to_str().and_then(from_file_uri).map_or_else(|| p.as_bytes().to_vec(), |d| d.into_os_string().into_vec());
        if verb == "trash" && o.paths.iter().any(|p| is_dot(&decoded(p))) {
            return Err("refusing to move “.” or “..” to the Trash".into());
        }
        match verb {
            "list" if o.paths.len() > 1 => Err("list takes one folder".into()),
            "info" | "trash" if o.paths.is_empty() => Err("no PATH given".into()),
            _ => Ok(o),
        }
    }
}

/// Whether a path ends in `.` or `..`, trailing slashes aside.
fn is_dot(p: &[u8]) -> bool {
    let mut end = p.len();
    while end > 1 && p[end - 1] == b'/' {
        end -= 1;
    }
    let last = p[..end].rsplit(|&b| b == b'/').next().unwrap_or_default();
    last == b"." || last == b".."
}

/// What failed, for stderr and the JSON's `errors`.
#[derive(Default)]
struct Report {
    errors: Vec<(Option<PathBuf>, String)>,
}

impl Report {
    fn fail(&mut self, path: Option<&Path>, message: impl Into<String>) {
        self.errors.push((path.map(Path::to_path_buf), message.into()));
    }
}

/// A command's output: its JSON fields after `version` and `command`, and its text.
struct Doc {
    fields: Vec<(&'static str, J)>,
    text: String,
}

/// An argument as a path: a `file://` URI is taken as one, and a relative path is relative to
/// the current folder. `.` parts and trailing slashes go, so a link is never followed by
/// accident; `..` stays for the kernel to resolve.
fn path_arg(env: &Env, a: &OsStr) -> PathBuf {
    let p = a.to_str().and_then(from_file_uri).unwrap_or_else(|| PathBuf::from(a));
    env.cwd.join(p).components().collect()
}

/// As [`path_arg`], with `..` taken off lexically too, as the window takes a typed path.
fn clean_arg(env: &Env, a: &OsStr) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path_arg(env, a).components() {
        match c {
            std::path::Component::ParentDir => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    out
}

/// Reads a folder, or the Trash, as the window would, and waits for the final result.
fn load(req: LoadRequest) -> Result<Loaded, String> {
    let (tx, rx) = mpsc::channel();
    let _handle = spawn_load(req, move |e| {
        let _ = tx.send(e);
    });
    loop {
        match rx.recv() {
            Ok(LoadEvent::Ready(l)) if l.settled => return Ok(l),
            Ok(LoadEvent::Ready(_)) => {}
            Ok(LoadEvent::Failed { error, .. }) => return Err(error),
            Err(_) => return Err("Reading this folder failed unexpectedly".into()),
        }
    }
}

fn request(dir: PathBuf, sort: SortSpec, show_hidden: bool, trash: Option<TrashSpec>) -> LoadRequest {
    LoadRequest { dir, generation: 0, sort, show_hidden, filter: String::new(), first_row: 0, row_count: 0, slow_read: None, slow_stat: None, trash, recent: false, search: None, search_skips_trash: None }
}

fn list(env: &mut Env, o: &Opts, report: &mut Report) -> Doc {
    let dir = o.paths.first().map_or_else(|| env.cwd.clone(), |p| clean_arg(env, p));
    // The order the window would show it in, unless one is asked for.
    let mut sort = match o.sort {
        Some(k) => k.first_order(),
        None => env.sorts.for_dir(&dir),
    };
    sort.descending ^= o.reverse;
    let mut items = Vec::new();
    match load(request(dir.clone(), sort, o.hidden, None)) {
        Ok(l) => {
            if let Some(e) = &l.listing.read_error {
                report.fail(Some(&dir), format!("Only part of the folder could be read: {e}"));
            }
            for &i in &l.rows {
                items.push(Item::read(l.listing.path(i), l.listing.name(i).to_vec(), None));
            }
        }
        Err(e) => report.fail(Some(&dir), e),
    }
    let mut text = String::new();
    for it in &items {
        text.push_str(&it.line());
    }
    let fields = vec![
        ("folder", J::path(&dir)),
        ("folder_uri", J::Str(to_file_uri(&dir))),
        ("sort", J::Obj(vec![("key", J::str(key_word(sort.key))), ("descending", J::Bool(sort.descending))])),
        ("hidden", J::Bool(o.hidden)),
        ("items", J::Arr(items.iter().map(Item::json).collect())),
    ];
    Doc { fields, text }
}

fn info(env: &mut Env, o: &Opts, report: &mut Report) -> Doc {
    let mut items = Vec::new();
    for a in &o.paths {
        let path = path_arg(env, a);
        if let Err(e) = std::fs::symlink_metadata(&path) {
            report.fail(Some(&path), crate::listing::describe(&e));
            continue;
        }
        let in_trash = trash_record(&env.trash, &path);
        let name = path.file_name().unwrap_or(path.as_os_str()).as_bytes().to_vec();
        items.push(Item::read(path, name, in_trash));
    }
    let text = items.iter().map(Item::block).collect::<Vec<_>>().join("\n");
    Doc { fields: vec![("items", J::Arr(items.iter().map(Item::json).collect()))], text }
}

/// For an item at the top of one of the Trash's `files` folders, what its info file says, or
/// `Some(None)` if it can't be read. `None` for anything else.
fn trash_record(cfg: &TrashConfig, path: &Path) -> Option<Option<Info>> {
    let canon = path.parent().and_then(|p| std::fs::canonicalize(p).ok()).zip(path.file_name()).map(|(p, n)| p.join(n));
    let info = [Some(path.to_path_buf()), canon].into_iter().flatten().find_map(|p| trash::info_file_for(cfg, &p))?;
    let root = info.parent()?.parent()?;
    // `Path=` is relative to the disk's top: the folder above `.Trash-UID`, or above `.Trash/UID`.
    let relative_to = if root == cfg.home_trash {
        None
    } else if root.file_name()?.as_bytes().starts_with(b".Trash-") {
        root.parent()
    } else {
        root.parent()?.parent()
    };
    Some(std::fs::read(&info).ok().and_then(|t| trash::parse_info(&t, relative_to)))
}

/// Runs one job in the operations engine, as the window would, and waits for it.
fn run_job(env: &Env, request: Request, report: &mut Report) -> Option<Outcome> {
    let (tx, rx) = mpsc::channel();
    let engine = Engine::new(env.trash.clone(), Arc::new(move |e| {
        let _ = tx.send(e);
    }));
    engine.start(request);
    // Only the job's thread can send now, so a job that dies ends the wait instead of hanging it.
    drop(engine);
    while let Ok(e) = rx.recv() {
        match e {
            Event::Progress(_) => {}
            // Neither trashing nor restoring asks, but nothing must wait on an answer forever.
            Event::Conflict(c) => {
                let _ = c.reply.send(Answer { choice: Choice::Skip, apply_to_all: true });
            }
            // The window's advice about Shift+Delete means nothing here.
            Event::Error { path, message, .. } => report.fail(Some(&path), message.trim_end_matches(" Shift+Delete deletes for good.")),
            Event::Finished(o) => return Some(o),
        }
    }
    report.fail(None, "the operation stopped unexpectedly");
    None
}

fn moved_json(original: &Path, in_trash: &Path) -> J {
    J::Obj(vec![("path", J::path(original)), ("uri", J::Str(to_file_uri(original))), ("trash_path", J::path(in_trash)), ("trash_uri", J::Str(to_file_uri(in_trash)))])
}

fn trash_cmd(env: &mut Env, o: &Opts, report: &mut Report) -> Doc {
    let paths: Vec<PathBuf> = o.paths.iter().map(|a| path_arg(env, a)).collect();
    let outcome = run_job(env, Request::Trash { paths }, report);
    let trashed: Vec<Trashed> = outcome.and_then(|o| o.undo).map_or_else(Vec::new, |u| {
        u.0.into_iter()
            .filter_map(|s| match s {
                Step::Trashed { item, .. } => Some(item),
                _ => None,
            })
            .collect()
    });
    let text = trashed.iter().map(|t| format!("Moved {} to the Trash, as {}\n", t.original.display(), t.file.display())).collect();
    Doc { fields: vec![("trashed", J::Arr(trashed.iter().map(|t| moved_json(&t.original, &t.file)).collect()))], text }
}

/// Every item in the Trash, on every disk, in the window's order for the Trash.
fn load_trash(env: &mut Env) -> Result<Loaded, String> {
    let files = env.trash.home_trash.join("files");
    let sort = env.sorts.for_dir(&files);
    let spec = TrashSpec { home_trash: env.trash.home_trash.clone(), uid: env.trash.uid, mounts: Some(env.mounts.clone()), disks_give_up: env.disks_give_up };
    load(request(files, sort, true, Some(spec)))
}

fn restore(env: &mut Env, o: &Opts, report: &mut Report) -> Doc {
    let loaded = match load_trash(env) {
        Ok(l) => l,
        Err(e) => {
            report.fail(Some(&env.trash.home_trash), e);
            let key = if o.paths.is_empty() { "items" } else { "restored" };
            return Doc { fields: vec![(key, J::Arr(Vec::new()))], text: String::new() };
        }
    };
    let l = &loaded.listing;
    if let Some(e) = &l.read_error {
        report.fail(Some(&env.trash.home_trash), format!("Only part of the Trash could be read: {e}"));
    }
    if o.paths.is_empty() {
        let items: Vec<Item> = loaded.rows.iter().map(|&i| Item::read(l.path(i), l.name(i).to_vec(), Some(l.trash_info(i).cloned()))).collect();
        let text = items.iter().map(Item::trash_line).collect();
        return Doc { fields: vec![("items", J::Arr(items.iter().map(Item::json).collect()))], text };
    }
    let mut picked: Vec<Trashed> = Vec::new();
    for a in &o.paths {
        let want = clean_arg(env, a);
        match find_in_trash(l, &loaded.rows, &want, &env.trash.home_trash, env.trash.uid) {
            Ok(t) if picked.iter().any(|p| p.file == t.file) => {}
            Ok(t) => picked.push(t),
            Err(e) => report.fail(Some(&want), e),
        }
    }
    let mut restored = Vec::new();
    if !picked.is_empty() {
        let outcome = run_job(env, Request::Restore { items: picked.clone() }, report);
        let created = outcome.map(|o| o.created).unwrap_or_default();
        // Each item that's now where it came from and gone from the Trash. Two items from one
        // path can both be asked for, and only the first can come back.
        restored = picked.into_iter().filter(|t| created.contains(&t.original) && std::fs::symlink_metadata(&t.file).is_err()).collect();
    }
    let text = restored.iter().map(|t| format!("Restored {}\n", t.original.display())).collect();
    Doc { fields: vec![("restored", J::Arr(restored.iter().map(|t| moved_json(&t.original, &t.file)).collect()))], text }
}

/// The item `want` names: its path in the Trash, or the path it came from, the latest deleted
/// if several did.
fn find_in_trash(l: &crate::listing::Listing, rows: &[u32], want: &Path, home_trash: &Path, uid: u32) -> Result<Trashed, String> {
    let unreadable = "Its record in the Trash is missing or can't be read, so it can't be put back";
    if let Some(&i) = rows.iter().find(|&&i| l.path(i) == want) {
        return l.trashed(i).ok_or_else(|| unreadable.to_string());
    }
    // The same item by another path, through a link to the Trash folder or the disk. Only for a
    // path that really is in a Trash: a hard link elsewhere is the same inode, but not in it.
    let id = |p: &Path| std::fs::symlink_metadata(p).ok().map(|m| (m.dev(), m.ino()));
    let real = want.parent().and_then(|p| std::fs::canonicalize(p).ok()).zip(want.file_name()).map(|(p, n)| p.join(n));
    if real.as_deref().is_some_and(|r| trash::is_inside_trash(home_trash, uid, r))
        && let Some(key) = id(want)
        && let Some(&i) = rows.iter().find(|&&i| id(&l.path(i)) == Some(key))
    {
        return l.trashed(i).ok_or_else(|| unreadable.to_string());
    }
    // Where it came from. The record has the real folder, which a link in `want` would hide.
    let latest = rows.iter().filter(|&&i| l.original(i).is_some_and(|o| o == want || Some(o) == real.as_deref())).max_by_key(|&&i| (l.trash_info(i).and_then(|t| t.deleted), i));
    match latest {
        Some(&i) => l.trashed(i).ok_or_else(|| unreadable.to_string()),
        None => Err("Not in the Trash".into()),
    }
}

fn key_word(k: SortKey) -> &'static str {
    match k {
        SortKey::Name => "name",
        SortKey::Size => "size",
        SortKey::Modified => "modified",
    }
}

/// One item, as `list`, `info` and `restore` describe it.
struct Item {
    name: Vec<u8>,
    path: PathBuf,
    meta: Option<std::fs::Metadata>,
    target: Option<PathBuf>,
    /// What a link leads to: `file`, `folder`, `other` or `missing`.
    target_kind: Option<&'static str>,
    /// For an item in the Trash: what its info file says, if it could be read.
    trash: Option<Option<Info>>,
}

impl Item {
    fn read(path: PathBuf, name: Vec<u8>, trash: Option<Option<Info>>) -> Item {
        let meta = std::fs::symlink_metadata(&path).ok();
        let is_link = meta.as_ref().is_some_and(|m| m.file_type().is_symlink());
        let target = is_link.then(|| std::fs::read_link(&path).ok()).flatten();
        let target_kind = is_link.then(|| match std::fs::metadata(&path) {
            Ok(m) => kind_word(&m),
            Err(_) => "missing",
        });
        Item { name, path, meta, target, target_kind, trash }
    }

    /// When it was made, in seconds since the epoch, where the filesystem records it.
    fn created(&self) -> Option<i64> {
        let t = self.meta.as_ref()?.created().ok()?;
        Some(match t.duration_since(std::time::UNIX_EPOCH) {
            Ok(d) => d.as_secs() as i64,
            Err(e) => -(e.duration().as_secs() as i64),
        })
    }

    fn kind(&self) -> &'static str {
        self.meta.as_ref().map_or("missing", kind_word)
    }

    fn json(&self) -> J {
        let m = self.meta.as_ref();
        let time = |t: Option<i64>| t.map_or(J::Null, J::Int);
        let created = self.created();
        let trash = match &self.trash {
            None => J::Null,
            Some(info) => J::Obj(vec![
                ("original", info.as_ref().map_or(J::Null, |i| J::path(&i.original))),
                ("original_uri", info.as_ref().map_or(J::Null, |i| J::Str(to_file_uri(&i.original)))),
                ("deleted", time(info.as_ref().and_then(|i| i.deleted))),
            ]),
        };
        J::Obj(vec![
            ("name", J::Str(String::from_utf8_lossy(&self.name).into_owned())),
            ("path", J::path(&self.path)),
            ("uri", J::Str(to_file_uri(&self.path))),
            ("kind", J::str(self.kind())),
            ("size", m.filter(|m| m.is_file()).map_or(J::Null, |m| J::Int(m.len() as i64))),
            ("modified", time(m.map(MetadataExt::mtime))),
            ("accessed", time(m.map(MetadataExt::atime))),
            ("changed", time(m.map(MetadataExt::ctime))),
            ("created", time(created)),
            ("mode", m.map_or(J::Null, |m| J::Str(format!("{:04o}", m.mode() & 0o7777)))),
            ("hidden", J::Bool(self.name.first() == Some(&b'.'))),
            ("target", self.target.as_deref().map_or(J::Null, J::path)),
            ("target_kind", self.target_kind.map_or(J::Null, J::str)),
            ("trash", trash),
        ])
    }

    /// The shown name: folders end in `/`, and links say where they lead.
    fn shown_name(&self) -> String {
        let mut s = String::from_utf8_lossy(&self.name).into_owned();
        if let Some(t) = &self.target {
            s.push_str(" -> ");
            s.push_str(&t.to_string_lossy());
        }
        if self.kind() == "folder" || self.target_kind == Some("folder") {
            s.push('/');
        }
        s
    }

    /// A row of `list`: size in bytes, modified, then the name.
    fn line(&self) -> String {
        let m = self.meta.as_ref();
        let size = m.filter(|m| m.is_file()).map_or_else(|| "-".to_string(), |m| m.len().to_string());
        format!("{size:>12}  {}  {}\n", m.map_or_else(|| "-".repeat(16), |m| date(m.mtime())), self.shown_name())
    }

    /// A row of the Trash: when it was deleted, where from, and where it is now.
    fn trash_line(&self) -> String {
        let info = self.trash.as_ref().and_then(Option::as_ref);
        let when = info.and_then(|i| i.deleted).map_or_else(|| "-".repeat(16), date);
        let from = info.map_or_else(|| format!("{} (where from is unknown)", String::from_utf8_lossy(&self.name)), |i| i.original.display().to_string());
        format!("{when}  {from}  [{}]\n", self.path.display())
    }

    /// `info`'s description.
    fn block(&self) -> String {
        let mut s = format!("{}\n", self.path.display());
        let mut row = |k: &str, v: String| s.push_str(&format!("  {k:<10}{v}\n"));
        row("kind", self.kind().to_string());
        if let Some(t) = &self.target {
            row("leads to", format!("{} ({})", t.display(), self.target_kind.unwrap_or("missing")));
        }
        if let Some(m) = &self.meta {
            if m.is_file() {
                row("size", format!("{} bytes", m.len()));
            }
            row("modified", date(m.mtime()));
            row("accessed", date(m.atime()));
            row("changed", date(m.ctime()));
            if let Some(t) = self.created() {
                row("created", date(t));
            }
            row("mode", format!("{:04o}", m.mode() & 0o7777));
        }
        match &self.trash {
            Some(Some(i)) => {
                row("from", i.original.display().to_string());
                row("deleted", i.deleted.map_or_else(|| "unknown".to_string(), date));
            }
            Some(None) => row("from", "unknown: its record in the Trash is missing or can't be read".to_string()),
            None => {}
        }
        s
    }
}

fn kind_word(m: &std::fs::Metadata) -> &'static str {
    let t = m.file_type();
    if t.is_file() {
        "file"
    } else if t.is_dir() {
        "folder"
    } else if t.is_symlink() {
        "symlink"
    } else {
        "other"
    }
}

/// Local time, `YYYY-MM-DD hh:mm`.
fn date(seconds: i64) -> String {
    let t = crate::time::local(seconds);
    format!("{:04}-{:02}-{:02} {:02}:{:02}", t.year, t.month, t.day, t.hour, t.minute)
}

/// Just enough JSON to write: no reading, and objects keep their fields in order.
enum J {
    Null,
    Bool(bool),
    Int(i64),
    Str(String),
    Arr(Vec<J>),
    Obj(Vec<(&'static str, J)>),
}

impl J {
    fn str(s: &str) -> J {
        J::Str(s.to_string())
    }

    /// A path as text. One that isn't UTF-8 has U+FFFD in place of the bytes that aren't, so
    /// it's only for showing: the `uri` beside it is exact.
    fn path(p: &Path) -> J {
        J::Str(p.to_string_lossy().into_owned())
    }

    fn write(&self, out: &mut String) {
        match self {
            J::Null => out.push_str("null"),
            J::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            J::Int(n) => out.push_str(&n.to_string()),
            J::Str(s) => {
                out.push('"');
                for c in s.chars() {
                    match c {
                        '"' => out.push_str("\\\""),
                        '\\' => out.push_str("\\\\"),
                        '\n' => out.push_str("\\n"),
                        '\t' => out.push_str("\\t"),
                        '\r' => out.push_str("\\r"),
                        c if (c as u32) < 0x20 || c == '\u{7f}' || c == '\u{2028}' || c == '\u{2029}' => out.push_str(&format!("\\u{:04x}", c as u32)),
                        c => out.push(c),
                    }
                }
                out.push('"');
            }
            J::Arr(v) => {
                out.push('[');
                for (n, x) in v.iter().enumerate() {
                    if n > 0 {
                        out.push(',');
                    }
                    x.write(out);
                }
                out.push(']');
            }
            J::Obj(fields) => {
                out.push('{');
                for (n, (k, v)) in fields.iter().enumerate() {
                    if n > 0 {
                        out.push(',');
                    }
                    J::str(k).write(out);
                    out.push(':');
                    v.write(out);
                }
                out.push('}');
            }
        }
    }
}

impl std::fmt::Display for J {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut s = String::new();
        self.write(&mut s);
        f.write_str(&s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::Scratch;
    use std::os::unix::ffi::OsStringExt;

    struct T {
        s: Scratch,
        env: Env,
    }

    /// A home, a working folder and a second "disk" in a scratch folder. Everything under
    /// `disk` counts as on another disk, with its own Trash.
    fn setup() -> T {
        let s = Scratch::new();
        let root = std::fs::canonicalize(s.path()).unwrap();
        let (work, disk) = (root.join("work"), root.join("disk"));
        std::fs::create_dir_all(&work).unwrap();
        std::fs::create_dir_all(&disk).unwrap();
        let mut trash = TrashConfig::new(&root.join("data"), rustix::process::geteuid().as_raw());
        let d = disk.clone();
        trash.mount_of = Arc::new(move |p| p.starts_with(&d).then(|| d.clone()));
        let mounts = vec![Mount { point: disk, fs_type: "ext4".into(), source: "/dev/standin".into() }];
        let sorts = FolderSorts::new(root.join("state/sorts"), vec![root.join("Downloads")]);
        T { s, env: Env { cwd: work, trash, mounts, sorts, disks_give_up: Duration::from_secs(3) } }
    }

    impl T {
        fn root(&self) -> PathBuf {
            std::fs::canonicalize(self.s.path()).unwrap()
        }

        fn run(&mut self, verb: &str, args: &[&str]) -> (i32, String, String) {
            let args: Vec<OsString> = args.iter().map(OsString::from).collect();
            let (mut out, mut err) = (Vec::new(), Vec::new());
            let code = run(&mut self.env, verb, &args, &mut out, &mut err);
            (code, String::from_utf8(out).unwrap(), String::from_utf8(err).unwrap())
        }

        /// Runs with `--json` and reads what it printed.
        fn json(&mut self, verb: &str, args: &[&str]) -> (i32, V) {
            let mut all = vec!["--json"];
            all.extend_from_slice(args);
            let (code, out, _) = self.run(verb, &all);
            assert!(out.ends_with('\n') && out.matches('\n').count() == 1, "one line: {out}");
            (code, parse(&out))
        }
    }

    fn names(v: &V, key: &str) -> Vec<String> {
        v.get(key).arr().iter().map(|i| i.get("name").s().to_string()).collect()
    }

    #[test]
    fn list_is_in_the_windows_order() {
        let mut t = setup();
        let w = t.env.cwd.clone();
        for (name, size) in [("b10.txt", 30), ("B9.txt", 10), ("a.txt", 20), (".dot", 5)] {
            std::fs::write(w.join(name), vec![b'x'; size]).unwrap();
        }
        std::fs::create_dir(w.join("zfolder")).unwrap();
        let (code, v) = t.json("list", &[]);
        assert_eq!(code, 0);
        assert_eq!(names(&v, "items"), ["zfolder", "a.txt", "B9.txt", "b10.txt"], "folders first, numbers as numbers, no hidden");
        assert_eq!(v.get("sort").get("key").s(), "name");
        let (_, v) = t.json("list", &["--hidden", "--sort", "size"]);
        assert_eq!(names(&v, "items"), ["zfolder", "b10.txt", "a.txt", "B9.txt", ".dot"], "biggest first, as the window's first press");
        assert!(v.get("sort").get("descending").b());
        let (_, v) = t.json("list", &["--sort=size", "--reverse"]);
        assert_eq!(names(&v, "items"), ["zfolder", "B9.txt", "a.txt", "b10.txt"]);
        // An order chosen in the window, for this folder or one above it, is the one used.
        t.env.sorts.set(&t.root(), SortSpec { key: SortKey::Size, descending: false });
        let (_, v) = t.json("list", &[w.to_str().unwrap()]);
        assert_eq!(names(&v, "items"), ["zfolder", "B9.txt", "a.txt", "b10.txt"]);
        assert!(!v.get("sort").get("descending").b());
        // And a folder that starts newest first does.
        let dl = t.s.dir("Downloads");
        std::fs::write(dl.join("old"), b"").unwrap();
        std::fs::write(dl.join("new"), b"").unwrap();
        let old = std::fs::File::options().write(true).open(dl.join("old")).unwrap();
        old.set_modified(std::time::UNIX_EPOCH + Duration::from_secs(1_000_000)).unwrap();
        let (_, v) = t.json("list", &["../Downloads"]);
        assert_eq!(names(&v, "items"), ["new", "old"]);
        assert_eq!(v.get("sort").get("key").s(), "modified");
        // Listing never changes what's saved.
        assert_eq!(t.env.sorts.for_dir(&t.root().join("Downloads")), SortSpec { key: SortKey::Modified, descending: true });
    }

    #[test]
    fn list_text_shows_size_date_and_name() {
        let mut t = setup();
        std::fs::write(t.env.cwd.join("a.txt"), b"hello").unwrap();
        std::fs::create_dir(t.env.cwd.join("sub")).unwrap();
        std::os::unix::fs::symlink("sub", t.env.cwd.join("ln")).unwrap();
        let (code, out, err) = t.run("list", &[]);
        assert_eq!((code, err.as_str()), (0, ""));
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines[0].trim_start().starts_with('-') && lines[0].ends_with("  ln -> sub/"), "{out}");
        assert!(lines[1].ends_with("  sub/"), "{out}");
        assert!(lines[2].trim_start().starts_with("5  ") && lines[2].ends_with("  a.txt"), "{out}");
    }

    #[test]
    fn every_item_has_every_field_and_odd_names_keep_their_bytes() {
        let mut t = setup();
        let odd = t.env.cwd.join(std::ffi::OsString::from_vec(b"caf\xe9 \"q\"\n".to_vec()));
        std::fs::write(&odd, b"abc").unwrap();
        let (code, v) = t.json("list", &[]);
        assert_eq!(code, 0);
        assert_eq!(v.get("version").n(), 1);
        assert_eq!(v.get("command").s(), "list");
        assert_eq!(v.get("errors").arr().len(), 0);
        let item = &v.get("items").arr()[0];
        let keys: Vec<&str> = item.obj().iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["name", "path", "uri", "kind", "size", "modified", "accessed", "changed", "created", "mode", "hidden", "target", "target_kind", "trash"]);
        assert_eq!(item.get("name").s(), "caf\u{fffd} \"q\"\n");
        assert_eq!(item.get("kind").s(), "file");
        assert_eq!(item.get("size").n(), 3);
        assert_eq!(item.get("mode").s().len(), 4);
        assert!(matches!(item.get("trash"), V::Null));
        // The URI is exact, and taken as an argument.
        let uri = item.get("uri").s().to_string();
        assert!(uri.ends_with("/caf%E9%20%22q%22%0A"), "{uri}");
        let (code, v) = t.json("info", &[&uri]);
        assert_eq!(code, 0);
        assert_eq!(v.get("items").arr()[0].get("size").n(), 3);
    }

    #[test]
    fn info_reports_a_missing_path_and_goes_on() {
        let mut t = setup();
        std::fs::write(t.env.cwd.join("here"), b"1").unwrap();
        std::os::unix::fs::symlink("gone", t.env.cwd.join("broken")).unwrap();
        let (code, out, err) = t.run("info", &["nope", "here", "broken"]);
        assert_eq!(code, 1);
        assert_eq!(err, format!("gannet info: {}: No such file or directory\n", t.env.cwd.join("nope").display()));
        assert!(out.contains("  kind      file\n") && out.contains("  leads to  gone (missing)\n"), "{out}");
        let (code, v) = t.json("info", &["nope", "here", "broken"]);
        assert_eq!(code, 1);
        assert_eq!(names(&v, "items"), ["here", "broken"]);
        assert_eq!(v.get("items").arr()[1].get("kind").s(), "symlink");
        assert_eq!(v.get("items").arr()[1].get("target").s(), "gone");
        assert_eq!(v.get("items").arr()[1].get("target_kind").s(), "missing");
        let errors = v.get("errors").arr();
        assert_eq!(errors.len(), 1);
        assert!(errors[0].get("path").s().ends_with("/work/nope"));
        assert_eq!(errors[0].get("message").s(), "No such file or directory");
    }

    #[test]
    fn trash_then_restore_puts_it_back() {
        let mut t = setup();
        std::fs::write(t.env.cwd.join("a.txt"), b"mine").unwrap();
        std::fs::create_dir(t.env.cwd.join("dir")).unwrap();
        let (code, v) = t.json("trash", &["a.txt", "dir/"]);
        assert_eq!(code, 0);
        let trashed = v.get("trashed").arr();
        assert_eq!(trashed.len(), 2);
        let in_trash = t.root().join("data/Trash/files/a.txt");
        assert_eq!(trashed[0].get("trash_path").s(), in_trash.to_str().unwrap());
        assert_eq!(trashed[0].get("path").s(), t.env.cwd.join("a.txt").to_str().unwrap());
        assert!(!t.env.cwd.join("a.txt").exists() && in_trash.exists());
        // Listed with where it came from, and described the same way.
        let (code, v) = t.json("restore", &[]);
        assert_eq!(code, 0);
        let items = v.get("items").arr();
        assert_eq!(names(&v, "items"), ["dir", "a.txt"]);
        assert_eq!(items[1].get("trash").get("original").s(), t.env.cwd.join("a.txt").to_str().unwrap());
        assert!((items[1].get("trash").get("deleted").n() - crate::time::now()).abs() < 60);
        let (_, v) = t.json("info", &[in_trash.to_str().unwrap()]);
        assert_eq!(v.get("items").arr()[0].get("trash").get("original").s(), t.env.cwd.join("a.txt").to_str().unwrap());
        // Back by its original path, and by its path in the Trash.
        let (code, out, err) = t.run("restore", &["a.txt", t.root().join("data/Trash/files/dir").to_str().unwrap()]);
        assert_eq!((code, err.as_str()), (0, ""));
        assert_eq!(out, format!("Restored {}\nRestored {}\n", t.env.cwd.join("a.txt").display(), t.env.cwd.join("dir").display()));
        assert_eq!(std::fs::read(t.env.cwd.join("a.txt")).unwrap(), b"mine");
        assert!(t.env.cwd.join("dir").is_dir());
        let (_, v) = t.json("restore", &[]);
        assert_eq!(v.get("items").arr().len(), 0);
    }

    #[test]
    fn restore_never_overwrites() {
        let mut t = setup();
        std::fs::write(t.env.cwd.join("a.txt"), b"old").unwrap();
        assert_eq!(t.run("trash", &["a.txt"]).0, 0);
        std::fs::write(t.env.cwd.join("a.txt"), b"new").unwrap();
        let (code, v) = t.json("restore", &["a.txt"]);
        assert_eq!(code, 1);
        assert_eq!(v.get("restored").arr().len(), 0);
        assert!(v.get("errors").arr()[0].get("message").s().starts_with("Something with this name is already in "));
        assert_eq!(std::fs::read(t.env.cwd.join("a.txt")).unwrap(), b"new");
        assert_eq!(std::fs::read(t.root().join("data/Trash/files/a.txt")).unwrap(), b"old");
    }

    #[test]
    fn restore_says_what_isnt_in_the_trash() {
        let mut t = setup();
        let (code, out, err) = t.run("restore", &["never-trashed"]);
        assert_eq!((code, out.as_str()), (1, ""));
        assert_eq!(err, format!("gannet restore: {}: Not in the Trash\n", t.env.cwd.join("never-trashed").display()));
        let (code, _, err) = t.run("trash", &["nope"]);
        assert_eq!(code, 1);
        assert!(err.starts_with("gannet trash: ") && err.contains("nope"), "{err}");
    }

    #[test]
    fn of_several_from_one_path_the_latest_deleted_comes_back() {
        let mut t = setup();
        let a = t.env.cwd.join("a.txt");
        std::fs::write(&a, b"first").unwrap();
        t.run("trash", &["a.txt"]);
        std::fs::write(&a, b"second").unwrap();
        t.run("trash", &["a.txt"]);
        // Dated a day apart, as both were trashed within a second.
        let info = t.root().join("data/Trash/info/a.txt.trashinfo");
        let text = std::fs::read_to_string(&info).unwrap();
        let date = text.lines().find_map(|l| l.strip_prefix("DeletionDate=")).unwrap().to_string();
        let earlier = crate::time::local(crate::time::from_local(crate::time::Local::parse_rfc3339_local(&date).unwrap()).unwrap() - 86400).rfc3339_local();
        std::fs::write(&info, text.replace(&date, &earlier)).unwrap();
        assert_eq!(t.run("restore", &["a.txt"]).0, 0);
        assert_eq!(std::fs::read(&a).unwrap(), b"second");
        assert_eq!(std::fs::read(t.root().join("data/Trash/files/a.txt")).unwrap(), b"first");
    }

    #[test]
    fn another_disks_trash_is_used_and_listed() {
        let mut t = setup();
        t.env.trash.ignore_home_trash = true;
        let f = t.s.file("disk/sub/x.txt", b"on the disk");
        let f = std::fs::canonicalize(f).unwrap();
        let (code, v) = t.json("trash", &[f.to_str().unwrap()]);
        assert_eq!(code, 0, "{:?}", v.get("errors").arr().iter().map(|e| e.get("message").s().to_string()).collect::<Vec<_>>());
        let uid = t.env.trash.uid;
        let in_trash = t.root().join(format!("disk/.Trash-{uid}/files/x.txt"));
        assert_eq!(v.get("trashed").arr()[0].get("trash_path").s(), in_trash.to_str().unwrap());
        let (_, v) = t.json("restore", &[]);
        let item = &v.get("items").arr()[0];
        assert_eq!(item.get("path").s(), in_trash.to_str().unwrap());
        assert_eq!(item.get("trash").get("original").s(), f.to_str().unwrap(), "relative to the disk's top");
        let (_, v) = t.json("info", &[in_trash.to_str().unwrap()]);
        assert_eq!(v.get("items").arr()[0].get("trash").get("original").s(), f.to_str().unwrap());
        assert_eq!(t.run("restore", &[f.to_str().unwrap()]).0, 0);
        assert_eq!(std::fs::read(&f).unwrap(), b"on the disk");
        assert!(!in_trash.exists());
    }

    #[test]
    fn bad_arguments_say_how_to_use_it() {
        let mut t = setup();
        for (verb, args) in [("list", &["--sort", "--hidden"][..]), ("list", &["--bogus"]), ("info", &[]), ("trash", &[]), ("list", &["a", "b"]), ("list", &["--sort", "colour"]), ("trash", &["--json=yes", "x"]), ("trash", &["--hidden", "x"])] {
            let (code, out, err) = t.run(verb, args);
            assert_eq!((code, out.as_str()), (1, ""), "{verb} {args:?}");
            assert!(err.contains(&format!("usage: gannet {verb}")), "{err}");
        }
        let (code, out, _) = t.run("restore", &["--help"]);
        assert_eq!(code, 0);
        assert!(out.starts_with("usage: gannet restore") && out.contains("Nothing is ever overwritten"));
        // After `--`, a name starting with a dash is a path.
        std::fs::write(t.env.cwd.join("-n"), b"").unwrap();
        assert_eq!(t.run("info", &["--", "-n"]).0, 0);
    }

    #[test]
    fn an_empty_or_dot_path_never_trashes_the_current_folder() {
        let mut t = setup();
        std::fs::write(t.env.cwd.join("keep"), b"").unwrap();
        let uri = |tail: &str| format!("file://{}/{tail}", crate::uri::encode_path(&t.env.cwd));
        let (dot, dotdot) = (uri("%2E"), uri("sub/%2e%2E/"));
        for arg in ["", ".", "./", "..", "../", "sub/.", "sub/..", &dot, &dotdot] {
            let (code, out, err) = t.run("trash", &[arg]);
            assert_eq!((code, out.as_str()), (1, ""), "{arg:?}");
            assert!(err.contains("usage: gannet trash"), "{arg:?}: {err}");
        }
        assert_eq!(t.run("info", &[""]).0, 1);
        assert_eq!(t.run("restore", &[""]).0, 1);
        assert!(t.env.cwd.join("keep").exists());
        assert!(std::fs::read_dir(t.root().join("data/Trash/files")).map_or(true, |mut d| d.next().is_none()));
        // A name that only starts with dots is a name.
        std::fs::write(t.env.cwd.join("..x"), b"").unwrap();
        assert_eq!(t.run("trash", &["..x"]).0, 0);
    }

    #[test]
    fn restored_lists_only_what_came_back() {
        let mut t = setup();
        let a = t.env.cwd.join("a.txt");
        std::fs::write(&a, b"first").unwrap();
        t.run("trash", &["a.txt"]);
        std::fs::write(&a, b"second").unwrap();
        t.run("trash", &["a.txt"]);
        let files = t.root().join("data/Trash/files");
        let (one, two) = (files.join("a.txt"), files.join("a.2.txt"));
        let (code, v) = t.json("restore", &[one.to_str().unwrap(), two.to_str().unwrap()]);
        assert_eq!(code, 1);
        let restored = v.get("restored").arr();
        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].get("trash_path").s(), one.to_str().unwrap());
        assert!(v.get("errors").arr()[0].get("path").s().ends_with("/work/a.txt"));
        assert!(two.exists());
    }

    #[test]
    fn a_hard_link_outside_the_trash_isnt_the_trashed_item() {
        let mut t = setup();
        std::fs::write(t.env.cwd.join("a.txt"), b"x").unwrap();
        std::fs::hard_link(t.env.cwd.join("a.txt"), t.env.cwd.join("b.txt")).unwrap();
        t.run("trash", &["a.txt"]);
        let (code, _, err) = t.run("restore", &["b.txt"]);
        assert_eq!(code, 1);
        assert!(err.ends_with("b.txt: Not in the Trash\n"), "{err}");
        assert!(!t.env.cwd.join("a.txt").exists());
    }

    #[test]
    fn bad_arguments_with_json_still_print_json_and_a_failed_write_fails() {
        let mut t = setup();
        let (code, v) = t.json("list", &["--sort", "colour"]);
        assert_eq!(code, 1);
        assert!(matches!(v.get("errors").arr()[0].get("path"), V::Null));
        assert!(v.get("errors").arr()[0].get("message").s().starts_with("--sort takes name, size or modified"));
        struct Full;
        impl Write for Full {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::from_raw_os_error(libc::ENOSPC))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        std::fs::write(t.env.cwd.join("a"), b"").unwrap();
        let mut err = Vec::new();
        assert_eq!(run(&mut t.env, "list", &[], &mut Full, &mut err), 1);
        assert!(String::from_utf8(err).unwrap().starts_with("gannet list: couldn't write the output: "));
    }

    #[test]
    fn json_strings_are_escaped() {
        let s = J::Obj(vec![("a", J::str("q\"\\\n\t\u{1}\u{2028}é")), ("b", J::Arr(vec![J::Null, J::Bool(true), J::Int(-3)]))]).to_string();
        assert_eq!(s, r#"{"a":"q\"\\\n\t\u0001\u2028é","b":[null,true,-3]}"#);
        assert_eq!(parse(&s).get("a").s(), "q\"\\\n\t\u{1}\u{2028}é");
    }

    /// What the tests read the JSON back into. Strict enough to catch output that isn't JSON.
    #[derive(Debug)]
    enum V {
        Null,
        Bool(bool),
        Num(i64),
        Str(String),
        Arr(Vec<V>),
        Obj(Vec<(String, V)>),
    }

    impl V {
        fn get(&self, k: &str) -> &V {
            self.obj().iter().find(|(n, _)| n == k).map(|(_, v)| v).unwrap_or_else(|| panic!("no {k} in {self:?}"))
        }
        fn obj(&self) -> &[(String, V)] {
            if let V::Obj(o) = self { o } else { panic!("not an object: {self:?}") }
        }
        fn arr(&self) -> &[V] {
            if let V::Arr(a) = self { a } else { panic!("not an array: {self:?}") }
        }
        fn s(&self) -> &str {
            if let V::Str(s) = self { s } else { panic!("not a string: {self:?}") }
        }
        fn n(&self) -> i64 {
            if let V::Num(n) = self { *n } else { panic!("not a number: {self:?}") }
        }
        fn b(&self) -> bool {
            if let V::Bool(b) = self { *b } else { panic!("not a bool: {self:?}") }
        }
    }

    fn parse(text: &str) -> V {
        let mut chars: Vec<char> = text.trim_end_matches('\n').chars().collect();
        chars.reverse();
        let v = value(&mut chars);
        assert!(chars.is_empty(), "left over: {:?}", chars.iter().rev().collect::<String>());
        v
    }

    fn value(c: &mut Vec<char>) -> V {
        let next = c.pop().expect("more JSON");
        match next {
            'n' | 't' | 'f' => {
                let mut word = next.to_string();
                while c.last().is_some_and(|x| x.is_ascii_alphabetic()) {
                    word.push(c.pop().unwrap());
                }
                match word.as_str() {
                    "null" => V::Null,
                    "true" => V::Bool(true),
                    "false" => V::Bool(false),
                    w => panic!("bad word {w}"),
                }
            }
            '-' | '0'..='9' => {
                let mut num = next.to_string();
                while c.last().is_some_and(|x| x.is_ascii_digit()) {
                    num.push(c.pop().unwrap());
                }
                V::Num(num.parse().unwrap())
            }
            '"' => {
                let mut s = String::new();
                loop {
                    match c.pop().expect("closing quote") {
                        '"' => return V::Str(s),
                        '\\' => match c.pop().unwrap() {
                            'n' => s.push('\n'),
                            't' => s.push('\t'),
                            'r' => s.push('\r'),
                            '"' => s.push('"'),
                            '\\' => s.push('\\'),
                            '/' => s.push('/'),
                            'u' => {
                                let hex: String = (0..4).map(|_| c.pop().unwrap()).collect();
                                s.push(char::from_u32(u32::from_str_radix(&hex, 16).unwrap()).unwrap());
                            }
                            e => panic!("bad escape {e}"),
                        },
                        ch if (ch as u32) < 0x20 => panic!("raw control character in a string"),
                        ch => s.push(ch),
                    }
                }
            }
            '[' => {
                let mut v = Vec::new();
                if c.last() == Some(&']') {
                    c.pop();
                    return V::Arr(v);
                }
                loop {
                    v.push(value(c));
                    match c.pop() {
                        Some(',') => {}
                        Some(']') => return V::Arr(v),
                        x => panic!("bad array at {x:?}"),
                    }
                }
            }
            '{' => {
                let mut o = Vec::new();
                if c.last() == Some(&'}') {
                    c.pop();
                    return V::Obj(o);
                }
                loop {
                    let V::Str(k) = value(c) else { panic!("key isn't a string") };
                    assert_eq!(c.pop(), Some(':'));
                    o.push((k, value(c)));
                    match c.pop() {
                        Some(',') => {}
                        Some('}') => return V::Obj(o),
                        x => panic!("bad object at {x:?}"),
                    }
                }
            }
            x => panic!("unexpected {x:?}"),
        }
    }
}
