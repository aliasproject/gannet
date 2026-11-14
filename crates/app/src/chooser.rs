//! Open and Save dialogs for other programs: a Gannet window in chooser mode.
//!
//! `gannet --choose ...` is the window. The portal service (`portal.rs`) starts one for each
//! request and reads what was picked from its output. The window is the ordinary file list,
//! with a bar along the bottom for the name to save as, the file types to show, and the
//! buttons that finish it.

use std::ffi::OsString;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rustix::fd::OwnedFd;

use gannet_core::listing::Listing;
use gannet_core::mime::{MimeDb, glob_match};
use gannet_core::ops;
use xkbcommon::xkb::keysyms as k;

use crate::app::{Action, App, Dialog, Hit, Menu, MenuItem};
use crate::canvas::{Canvas, Rect};
use crate::field::Field;
use crate::wl::Mods;

pub const ARG: &str = "--choose";
pub const APP_ID: &str = "com.aliasproject.Gannet.Chooser";
pub const USAGE: &str = "gannet --choose open|folder|save|save-files [--multiple] [--title TEXT] [--accept LABEL] [--folder DIR] [--name NAME] [--file PATH] [--save-name NAME]... [--filter LABEL [--glob PATTERN | --mime TYPE]...]... [--current N] [--parent wayland:HANDLE] [--modal]";
/// The window's size before the compositor says otherwise, in logical pixels.
pub const SIZE: (i32, i32) = (980, 640);

/// What the chooser is for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    /// Pick one file, or several with `multiple`.
    #[default]
    Open,
    /// Pick a folder, or several.
    Folder,
    /// Name one file to save.
    Save,
    /// Pick a folder to save several named files in.
    SaveFiles,
}

/// One entry in a filter: a shell pattern for names, or a file type.
#[derive(Clone, Debug, PartialEq)]
pub enum Pattern {
    Glob(String),
    Mime(String),
}

/// A choice in the file-type list, like "Images": a file shows if any of its patterns match.
#[derive(Clone, Debug, PartialEq)]
pub struct Filter {
    pub label: String,
    pub patterns: Vec<Pattern>,
}

/// Everything a request asks for. It's passed to the window as arguments.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Spec {
    pub mode: Mode,
    pub multiple: bool,
    pub title: String,
    /// The label of the button that finishes it, like "Upload".
    pub accept: Option<String>,
    /// Where to start.
    pub folder: Option<PathBuf>,
    /// The name to suggest when saving.
    pub name: Option<String>,
    /// A file being saved again: start in its folder, with its name.
    pub file: Option<PathBuf>,
    /// For saving several files, their names.
    pub save_names: Vec<PathBuf>,
    pub filters: Vec<Filter>,
    /// Which filter to start with.
    pub current: Option<usize>,
    /// The window to sit above, as `wayland:HANDLE`.
    pub parent: Option<String>,
    pub modal: bool,
}

impl Spec {
    /// The arguments after `--choose` that make a window show this. Every value is its own
    /// argument, so nothing needs quoting.
    pub fn args(&self) -> Vec<OsString> {
        let mut a: Vec<OsString> = vec![ARG.into()];
        a.push(match self.mode {
            Mode::Open => "open",
            Mode::Folder => "folder",
            Mode::Save => "save",
            Mode::SaveFiles => "save-files",
        }.into());
        let mut pair = |flag: &str, v: OsString| {
            a.push(flag.into());
            a.push(v);
        };
        if !self.title.is_empty() {
            pair("--title", self.title.clone().into());
        }
        if let Some(l) = &self.accept {
            pair("--accept", l.into());
        }
        if let Some(f) = &self.folder {
            pair("--folder", f.clone().into());
        }
        if let Some(n) = &self.name {
            pair("--name", n.into());
        }
        if let Some(f) = &self.file {
            pair("--file", f.clone().into());
        }
        for n in &self.save_names {
            pair("--save-name", n.clone().into());
        }
        for f in &self.filters {
            pair("--filter", f.label.clone().into());
            for p in &f.patterns {
                match p {
                    Pattern::Glob(g) => pair("--glob", g.into()),
                    Pattern::Mime(m) => pair("--mime", m.into()),
                }
            }
        }
        if let Some(c) = self.current {
            pair("--current", c.to_string().into());
        }
        if let Some(p) = &self.parent {
            pair("--parent", p.into());
        }
        if self.multiple {
            a.push("--multiple".into());
        }
        if self.modal {
            a.push("--modal".into());
        }
        a
    }

    /// Reads the arguments after `--choose`. `None` if they don't make sense.
    pub fn parse(args: impl IntoIterator<Item = OsString>) -> Option<Self> {
        let mut it = args.into_iter();
        let mode = match it.next()?.to_str()? {
            "open" => Mode::Open,
            "folder" => Mode::Folder,
            "save" => Mode::Save,
            "save-files" => Mode::SaveFiles,
            _ => return None,
        };
        let mut s = Spec { mode, ..Spec::default() };
        let text = |v: Option<OsString>| v.map(|v| v.to_string_lossy().into_owned());
        while let Some(flag) = it.next() {
            match flag.to_str()? {
                "--multiple" => s.multiple = true,
                "--modal" => s.modal = true,
                "--title" => s.title = text(it.next())?,
                "--accept" => s.accept = Some(text(it.next())?),
                "--folder" => s.folder = Some(it.next()?.into()),
                "--name" => s.name = Some(text(it.next())?),
                "--file" => s.file = Some(it.next()?.into()),
                "--save-name" => s.save_names.push(it.next()?.into()),
                "--filter" => s.filters.push(Filter { label: text(it.next())?, patterns: Vec::new() }),
                "--glob" => s.filters.last_mut()?.patterns.push(Pattern::Glob(text(it.next())?)),
                "--mime" => s.filters.last_mut()?.patterns.push(Pattern::Mime(text(it.next())?)),
                "--current" => s.current = Some(text(it.next())?.parse().ok()?),
                "--parent" => s.parent = Some(text(it.next())?),
                _ => return None,
            }
        }
        s.current = s.current.filter(|&c| c < s.filters.len());
        Some(s)
    }

    /// The title when the request didn't give one.
    pub fn title(&self) -> String {
        if !self.title.is_empty() {
            return self.title.clone();
        }
        match self.mode {
            Mode::Open if self.multiple => "Open Files",
            Mode::Open => "Open File",
            Mode::Folder => "Select Folder",
            Mode::Save | Mode::SaveFiles => "Save",
        }
        .to_string()
    }

    pub fn accept_label(&self) -> String {
        if let Some(l) = self.accept.as_deref().map(without_mnemonics).filter(|l| !l.trim().is_empty()) {
            return l;
        }
        match self.mode {
            Mode::Open => "Open",
            Mode::Folder => "Select",
            Mode::Save | Mode::SaveFiles => "Save",
        }
        .to_string()
    }

    /// The folder to start in: the one the request named, or else the current file's, if
    /// it's there.
    pub fn start_folder(&self) -> Option<PathBuf> {
        let from_file = self.file.as_ref().and_then(|f| f.parent()).map(Path::to_path_buf);
        [self.folder.clone(), from_file].into_iter().flatten().find(|d| d.is_absolute() && d.is_dir())
    }

    /// The name the Save field starts with.
    pub fn start_name(&self) -> String {
        let from_file = self.file.as_ref().and_then(|f| f.file_name()).map(|n| n.to_string_lossy().into_owned());
        self.name.clone().or(from_file).unwrap_or_default()
    }
}

/// Which rows a chooser shows: every folder, so there's a way around, and the files the chosen
/// filter takes. A chooser for folders shows only folders.
pub struct RowFilter {
    folders_only: bool,
    globs: Vec<Vec<u8>>,
    mimes: Vec<String>,
    db: Option<Arc<MimeDb>>,
}

impl RowFilter {
    pub fn new(folders_only: bool, filter: Option<&Filter>, db: Option<Arc<MimeDb>>) -> Self {
        let (mut globs, mut mimes) = (Vec::new(), Vec::new());
        for p in filter.map_or(&[][..], |f| f.patterns.as_slice()) {
            match p {
                // Case doesn't matter, as users don't expect "*.jpg" to leave out "IMG.JPG".
                // Lowered the same way as the names they're matched against.
                Pattern::Glob(g) => globs.push(g.to_ascii_lowercase().into_bytes()),
                Pattern::Mime(m) => mimes.push(m.to_ascii_lowercase()),
            }
        }
        Self { folders_only, globs, mimes, db }
    }

    /// Whether this filter lets everything through anyway.
    pub fn takes_all(&self) -> bool {
        !self.folders_only && ((self.globs.is_empty() && self.mimes.is_empty()) || self.globs.iter().any(|g| g == b"*"))
    }

    pub fn keeps(&self, name: &[u8], folder: bool) -> bool {
        if folder || self.takes_all() {
            return folder || !self.folders_only;
        }
        if self.folders_only {
            return false;
        }
        let lower = name.to_ascii_lowercase();
        if self.globs.iter().any(|g| glob_match(g, &lower)) {
            return true;
        }
        let Some(db) = self.db.as_ref().filter(|_| !self.mimes.is_empty()) else { return false };
        let mime = db.guess(name, None);
        self.mimes.iter().any(|m| db.is_a(mime, m))
    }

    pub fn keep_rows(&self, l: &Listing, rows: Vec<u32>) -> Vec<u32> {
        if self.takes_all() {
            return rows;
        }
        rows.into_iter().filter(|&i| self.keeps(l.name(i), l.kind(i).is_dirlike())).collect()
    }
}

/// What the window says when it closes: `accept`, then the filter chosen and a `uri` line for
/// each path; or `cancel`.
pub fn outcome_text(picked: Option<&[PathBuf]>, filter: Option<usize>) -> String {
    let Some(paths) = picked else { return "cancel\n".into() };
    let mut s = String::from("accept\n");
    if let Some(f) = filter {
        s.push_str(&format!("filter {f}\n"));
    }
    for p in paths {
        s.push_str(&format!("uri {}\n", gannet_core::uri::to_file_uri(p)));
    }
    s
}

/// Reads what the window said: the URIs and filter if something was picked, `None` if not.
pub fn read_outcome(text: &str) -> Option<(Vec<String>, Option<usize>)> {
    let mut lines = text.lines();
    if lines.next()? != "accept" {
        return None;
    }
    let (mut uris, mut filter) = (Vec::new(), None);
    for line in lines {
        if let Some(u) = line.strip_prefix("uri ") {
            uris.push(u.to_string());
        } else if let Some(f) = line.strip_prefix("filter ") {
            filter = f.parse().ok();
        }
    }
    (!uris.is_empty()).then_some((uris, filter))
}

/// A chooser window's state, in the app.
pub struct Chooser {
    pub spec: Spec,
    /// The filter showing, by its place in `spec.filters`.
    pub filter: Option<usize>,
    /// The name to save as.
    pub name: Field,
    /// Whether typing goes to the name rather than the list.
    pub name_focused: bool,
    pub rows: Arc<RowFilter>,
    db: Option<Arc<MimeDb>>,
    /// What was picked, once it's done. `None` when closed without.
    pub picked: Option<Vec<PathBuf>>,
    /// The filter labels, for the menu, which takes labels that last.
    labels: Vec<&'static str>,
    /// Where the outcome goes.
    out: Option<OwnedFd>,
    /// The buttons and file-type list as last drawn, for the end-to-end checks to click.
    pub drawn: (Rect, Rect, Option<Rect>),
}

impl Chooser {
    /// `mime_dirs` are the file-type database's folders, read only if a filter names a type.
    pub fn new(spec: Spec, mime_dirs: &[PathBuf], out: Option<OwnedFd>) -> Self {
        let db = spec.filters.iter().flat_map(|f| &f.patterns).any(|p| matches!(p, Pattern::Mime(_))).then(|| Arc::new(MimeDb::load(mime_dirs)));
        let filter = spec.current.or((!spec.filters.is_empty()).then_some(0));
        let mut name = Field::new(&spec.start_name());
        // Ready to type over the name and keep its extension, as rename does.
        name.select_stem();
        let name_focused = spec.mode == Mode::Save;
        // A chooser lives for one request, so leaking its few labels costs nothing that lasts.
        let labels = spec.filters.iter().map(|f| &*Box::leak(f.label.clone().into_boxed_str())).collect();
        let folders_only = matches!(spec.mode, Mode::Folder | Mode::SaveFiles);
        let rows = Arc::new(RowFilter::new(folders_only, filter.and_then(|i| spec.filters.get(i)), db.clone()));
        Self { spec, filter, name, name_focused, rows, db, picked: None, labels, out, drawn: Default::default() }
    }

    fn set_filter(&mut self, i: usize) {
        self.filter = Some(i);
        let folders_only = matches!(self.spec.mode, Mode::Folder | Mode::SaveFiles);
        self.rows = Arc::new(RowFilter::new(folders_only, self.spec.filters.get(i), self.db.clone()));
    }

    /// Writes the outcome for the service, on what was standard output.
    pub fn finish(&mut self) {
        let text = outcome_text(self.picked.as_deref(), self.filter.filter(|_| self.picked.is_some()));
        if let Some(fd) = self.out.take() {
            let _ = std::fs::File::from(fd).write_all(text.as_bytes());
        }
    }
}

/// Takes standard output for the outcome alone, and points it at /dev/null for anything this
/// window starts, like a preview. The service reads until the window's copy closes, and one
/// left open in a program that outlives the window would keep it waiting.
pub fn take_stdout() -> Option<OwnedFd> {
    let out = rustix::io::fcntl_dupfd_cloexec(rustix::stdio::stdout(), 3).ok()?;
    if let Ok(null) = std::fs::File::options().write(true).open("/dev/null") {
        let _ = rustix::stdio::dup2_stdout(&null);
    }
    Some(out)
}

/// The parts of the bar along the bottom.
pub struct Bar {
    pub accept: Rect,
    pub cancel: Rect,
    pub filter: Option<Rect>,
    /// The name field, and where its label starts, when saving one file.
    pub name: Option<(Rect, i32)>,
    /// Where the text on the left may go, when there's no name field.
    pub info: Rect,
}

impl App {
    pub fn chooser_bar(&mut self) -> Bar {
        let l = self.layout();
        let bar = l.status;
        let Some(c) = self.chooser.as_ref() else { return Bar { accept: Rect::default(), cancel: Rect::default(), filter: None, name: None, info: Rect::default() } };
        let (accept_label, save, labels) = (c.spec.accept_label(), c.spec.mode == Mode::Save, c.labels.clone());
        let h = self.px(32.0);
        let y = bar.y + (bar.h - h) / 2;
        let button_w = |app: &mut App, s: &str| (app.text.measure(s) as i32 + app.px(36.0)).max(app.px(92.0));
        let w = button_w(self, &accept_label);
        let accept = Rect::new(bar.right() - self.px(14.0) - w, y, w, h);
        let w = button_w(self, "Cancel");
        let cancel = Rect::new(accept.x - self.px(8.0) - w, y, w, h);
        let mut left_end = cancel.x - self.px(16.0);
        let filter = (!labels.is_empty()).then(|| {
            let widest = labels.iter().map(|s| self.text.measure(s) as i32).max().unwrap_or(0);
            let w = (widest + self.px(44.0)).clamp(self.px(120.0), self.px(260.0));
            let r = Rect::new(left_end - w, y, w, h);
            left_end = r.x - self.px(16.0);
            r
        });
        let x0 = self.px(16.0);
        let name = save.then(|| {
            let label_w = self.text.measure("Name") as i32;
            let fx = x0 + label_w + self.px(10.0);
            (Rect::new(fx, y, (left_end - fx).clamp(self.px(80.0), self.px(520.0)).min((left_end - fx).max(0)), h), x0)
        });
        Bar { accept, cancel, filter, name, info: Rect::new(x0, bar.y, (left_end - x0).max(0), bar.h) }
    }

    /// The bar's part under a point, if it's one.
    pub fn chooser_hit(&mut self, x: i32, y: i32) -> Option<Hit> {
        self.chooser.as_ref()?;
        if !self.layout().status.contains(x, y) {
            return None;
        }
        let b = self.chooser_bar();
        Some(if b.accept.contains(x, y) {
            if self.chooser_can_accept() { Hit::ChooserAccept } else { Hit::None }
        } else if b.cancel.contains(x, y) {
            Hit::ChooserCancel
        } else if b.filter.is_some_and(|r| r.contains(x, y)) {
            Hit::ChooserFilter
        } else if b.name.is_some_and(|(r, _)| r.contains(x, y)) {
            Hit::ChooserName
        } else {
            Hit::None
        })
    }

    /// Rows the chooser's filter lets through.
    pub fn choosable(&self, l: &Listing, rows: Vec<u32>) -> Vec<u32> {
        match &self.chooser {
            Some(c) => c.rows.keep_rows(l, rows),
            None => rows,
        }
    }

    /// The rows an Open or Select would take: the selection, or the row under the cursor.
    fn chooser_targets(&self) -> Vec<u32> {
        let mut ids = self.selected_ids();
        if ids.is_empty() && self.cursor_shown {
            ids.extend(self.rows.get(self.cursor));
        }
        ids
    }

    /// Saving needs a real folder, which Recent and the Trash aren't.
    fn chooser_folder_ok(&self) -> bool {
        !self.is_recent_view() && !self.is_trash_view() && !self.in_trash()
    }

    /// Whether a chooser lets its list do `a` here, by menu or key alike. What changes the
    /// folder needs one it can change: not Recent, and not the Trash, where Delete would
    /// delete for good.
    pub(crate) fn chooser_allows(&self, a: Action) -> bool {
        let changes_folder = matches!(a, Action::Rename | Action::NewFolder | Action::Trash);
        allows(a) && (!changes_folder || self.chooser_folder_ok())
    }

    pub fn chooser_can_accept(&self) -> bool {
        let Some(c) = &self.chooser else { return false };
        let Some(l) = &self.listing else { return false };
        match c.spec.mode {
            Mode::Open => !self.chooser_targets().is_empty(),
            Mode::Folder | Mode::SaveFiles => self.chooser_folder_ok() || self.chooser_targets().iter().any(|&i| l.kind(i).is_dirlike()),
            Mode::Save => self.chooser_folder_ok() && !c.name.text.trim().is_empty(),
        }
    }

    /// Enter on rows, or a double click: a folder opens, and a file is picked.
    pub(crate) fn chooser_open(&mut self, ids: Vec<u32>) {
        let (Some(l), Some(c)) = (self.listing.clone(), self.chooser.as_ref()) else { return };
        let mode = c.spec.mode;
        let folders: Vec<u32> = ids.iter().copied().filter(|&i| l.kind(i).is_dirlike()).collect();
        let files: Vec<u32> = ids.iter().copied().filter(|&i| !l.kind(i).is_dirlike()).collect();
        match mode {
            Mode::Open if !files.is_empty() => self.chooser_accept(),
            Mode::Save if !files.is_empty() => {
                let name = String::from_utf8_lossy(l.name(files[0])).into_owned();
                if let Some(c) = self.chooser.as_mut() {
                    c.name = Field::new(&name);
                }
                self.chooser_accept();
            }
            _ => {
                if let Some(&i) = folders.first() {
                    self.navigate(l.path(i), None);
                }
            }
        }
    }

    /// The accept button: picks what's chosen, or asks first about replacing.
    pub(crate) fn chooser_accept(&mut self) {
        if !self.chooser_can_accept() {
            return;
        }
        let (Some(l), Some(c)) = (self.listing.clone(), self.chooser.as_ref()) else { return };
        let (mode, multiple) = (c.spec.mode, c.spec.multiple);
        let ids = self.chooser_targets();
        match mode {
            Mode::Open => {
                let folders: Vec<u32> = ids.iter().copied().filter(|&i| l.kind(i).is_dirlike()).collect();
                let mut files: Vec<PathBuf> = ids.iter().copied().filter(|&i| !l.kind(i).is_dirlike()).map(|i| l.path(i)).collect();
                if files.is_empty() {
                    // Only folders: Open goes into the first, as in other file choosers.
                    if let Some(&i) = folders.first() {
                        self.navigate(l.path(i), None);
                    }
                    return;
                }
                if !multiple {
                    files.truncate(1);
                }
                self.chooser_done(files);
            }
            Mode::Folder | Mode::SaveFiles => {
                let mut folders: Vec<PathBuf> = ids.iter().copied().filter(|&i| l.kind(i).is_dirlike()).map(|i| l.path(i)).collect();
                if folders.is_empty() {
                    folders.push(self.dir.clone());
                }
                if mode == Mode::Folder {
                    if !multiple {
                        folders.truncate(1);
                    }
                    return self.chooser_done(folders);
                }
                let dir = folders.swap_remove(0);
                let names = self.chooser.as_ref().map(|c| c.spec.save_names.clone()).unwrap_or_default();
                let paths: Vec<PathBuf> = names.iter().filter_map(|n| n.file_name()).map(|n| dir.join(n)).collect();
                if paths.is_empty() {
                    return self.notice("The program didn't name any files to save".to_string());
                }
                self.chooser_save(paths);
            }
            Mode::Save => {
                let typed = c.name.text.trim().to_string();
                let home = self.places.first().map(|p| p.path.clone());
                // A typed path goes where it says: "~/x", "/tmp/x" or "sub/x".
                let path = if typed.contains('/') {
                    let p = crate::app::expand_home(&typed, home.as_deref());
                    crate::app::clean(&if p.is_absolute() { p } else { self.dir.join(p) })
                } else {
                    crate::app::clean(&self.dir.join(&typed))
                };
                if path.is_dir() {
                    // A folder's name: go into it, as other file choosers do.
                    let rest = self.chooser.as_ref().map(|c| c.spec.start_name()).unwrap_or_default();
                    if let Some(c) = self.chooser.as_mut() {
                        c.name = Field::new(&rest);
                        c.name.select_stem();
                    }
                    return self.navigate(path, None);
                }
                let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else { return };
                if let Err(e) = ops::validate_name(&name.to_string_lossy()) {
                    return self.notice(e.to_string());
                }
                if !parent.is_dir() {
                    return self.notice(format!("There's no folder “{}” to save in", parent.display()));
                }
                self.chooser_save(vec![path]);
            }
        }
    }

    /// Saving: checks the folder can be written to, and asks before replacing anything.
    fn chooser_save(&mut self, paths: Vec<PathBuf>) {
        let Some(dir) = paths.first().and_then(|p| p.parent()).map(Path::to_path_buf) else { return };
        if rustix::fs::access(&dir, rustix::fs::Access::WRITE_OK).is_err() {
            return self.notice(format!("You can't save in “{}”. Choose another folder", self.crumb_label(&dir)));
        }
        if let Some(p) = paths.iter().find(|p| std::fs::symlink_metadata(p).is_ok_and(|m| m.is_dir())) {
            return self.notice(format!("“{}” is a folder, so it can't be saved over", p.file_name().unwrap_or_default().to_string_lossy()));
        }
        let existing: Vec<PathBuf> = paths.iter().filter(|p| std::fs::symlink_metadata(p).is_ok()).cloned().collect();
        if existing.is_empty() {
            return self.chooser_done(paths);
        }
        // Cancel has the focus, as replacing loses what's there.
        self.dialog = Some(Dialog::ConfirmReplace { paths, existing, at: 1 });
        self.invalidate_all();
    }

    pub(crate) fn chooser_done(&mut self, paths: Vec<PathBuf>) {
        if let Some(c) = self.chooser.as_mut() {
            c.picked = Some(paths);
            self.cmds.push(crate::app::Cmd::Quit);
        }
    }

    pub(crate) fn chooser_cancel(&mut self) {
        if let Some(c) = self.chooser.as_mut() {
            c.picked = None;
            self.cmds.push(crate::app::Cmd::Quit);
        }
    }

    /// A key the chooser handles before the list does. Returns whether it did.
    pub(crate) fn chooser_key(&mut self, sym: u32, utf8: &str, m: Mods) -> bool {
        let Some(c) = self.chooser.as_mut() else { return false };
        let save = c.spec.mode == Mode::Save;
        if sym == k::KEY_Tab || sym == k::KEY_ISO_Left_Tab {
            if save {
                c.name_focused = !c.name_focused;
                if !c.name_focused && !self.cursor_shown {
                    self.step(0, Mods::default());
                }
                self.invalidate_all();
            }
            return true;
        }
        // Shortcuts the field has no use for, like Ctrl+L, Ctrl+H or Alt+Up, still work.
        let editing = matches!(sym, k::KEY_a | k::KEY_c | k::KEY_x | k::KEY_v | k::KEY_z | k::KEY_Z | k::KEY_Left | k::KEY_Right | k::KEY_Home | k::KEY_End | k::KEY_BackSpace | k::KEY_Delete);
        if c.name_focused && (m.alt || m.logo || (m.ctrl && !editing)) {
            return false;
        }
        if c.name_focused {
            match sym {
                k::KEY_Escape => self.chooser_cancel(),
                k::KEY_Return | k::KEY_KP_Enter => self.chooser_accept(),
                // Up and down go to the list, as typing a name and then looking is common.
                k::KEY_Up | k::KEY_Down | k::KEY_Page_Up | k::KEY_Page_Down => {
                    c.name_focused = false;
                    self.invalidate_all();
                    return false;
                }
                _ => {
                    self.field_key(sym, utf8, m);
                    self.invalidate(self.layout().status);
                }
            }
            return true;
        }
        match sym {
            // Escape still closes the preview and clears a filter first, then it cancels.
            k::KEY_Escape if !self.key_is_repeat && self.previewer.is_none() && self.filter.is_empty() => {
                self.chooser_cancel();
                true
            }
            // With nothing picked, Enter takes the folder showing, where that's what's wanted.
            k::KEY_Return | k::KEY_KP_Enter if self.chooser_targets().is_empty() => {
                self.chooser_accept();
                true
            }
            _ => false,
        }
    }

    pub(crate) fn chooser_click(&mut self, hit: Hit, x: i32) {
        match hit {
            Hit::ChooserAccept => self.chooser_accept(),
            Hit::ChooserCancel => self.chooser_cancel(),
            Hit::ChooserFilter => self.open_filter_menu(),
            Hit::ChooserName => {
                let Some((r, _)) = self.chooser_bar().name else { return };
                let Some(c) = self.chooser.as_ref() else { return };
                let (text, caret) = (c.name.text.clone(), c.name.caret);
                let inner = r.w - self.px(16.0);
                let shift = (self.text.measure(&text[..caret]) - inner as f32 + 2.0).max(0.0);
                let at = self.text.offset_at(&text, (x - r.x - self.px(8.0)) as f32 + shift);
                if let Some(c) = self.chooser.as_mut() {
                    // The first click into it only gives it the keyboard, keeping its selection.
                    if c.name_focused {
                        c.name.move_to(at, false);
                    }
                    c.name_focused = true;
                }
                self.invalidate_all();
            }
            _ => {}
        }
    }

    /// Anything clicked outside the name field takes the keyboard back to the list. A file
    /// clicked while saving puts its name in the field, to save over it or start from it.
    pub(crate) fn chooser_clicked_list(&mut self, row: Option<usize>) {
        let Some(c) = self.chooser.as_mut() else { return };
        if c.name_focused {
            c.name_focused = false;
            self.invalidate(self.layout().status);
        }
        let (Some(row), Some(l)) = (row, self.listing.clone()) else { return };
        let Some(&id) = self.rows.get(row) else { return };
        if let Some(c) = self.chooser.as_mut()
            && c.spec.mode == Mode::Save
            && !l.kind(id).is_dirlike()
        {
            c.name = Field::new(&String::from_utf8_lossy(l.name(id)));
            self.invalidate(self.layout().status);
        }
    }

    fn open_filter_menu(&mut self) {
        let Some(r) = self.chooser_bar().filter else { return };
        let Some(c) = self.chooser.as_ref() else { return };
        let items: Vec<MenuItem> = c.labels.iter().enumerate().map(|(i, &label)| MenuItem { label, keys: "", action: Action::ChooseFilter(i), enabled: true }).collect();
        let at = c.filter.unwrap_or(usize::MAX);
        let h = self.px(28.0) * items.len() as i32 + self.px(8.0);
        // It opens upwards, over the list, from the right edge of the button.
        self.menu = Some(Menu { items, x: r.right() - self.px(250.0), y: r.y - h - self.px(4.0), at });
        self.invalidate_all();
    }

    pub(crate) fn choose_filter(&mut self, i: usize) {
        let Some(c) = self.chooser.as_mut() else { return };
        if c.filter == Some(i) || i >= c.spec.filters.len() {
            return;
        }
        c.set_filter(i);
        self.rederive(false);
        self.invalidate_all();
    }

    /// The menu in a chooser: what helps pick, and nothing that would surprise the program
    /// waiting on it.
    pub(crate) fn chooser_menu_items(&self) -> Vec<MenuItem> {
        let any = self.sel_count > 0;
        let label = self.chooser.as_ref().map_or("Open", |c| if matches!(c.spec.mode, Mode::Open | Mode::Save) { "Open" } else { "Open folder" });
        let writable = self.chooser_folder_ok();
        vec![
            MenuItem { label, keys: "Enter", action: Action::Open, enabled: any },
            MenuItem { label: "Preview", keys: "Space", action: Action::Preview, enabled: any },
            MenuItem { label: "Rename", keys: "F2", action: Action::Rename, enabled: self.sel_count == 1 && writable },
            MenuItem { label: "New folder", keys: "Ctrl+Shift+N", action: Action::NewFolder, enabled: writable },
            MenuItem { label: "Move to Trash", keys: "Delete", action: Action::Trash, enabled: any && writable },
            MenuItem { label: if self.show_hidden { "Hide hidden files" } else { "Show hidden files" }, keys: "Ctrl+H", action: Action::ToggleHidden, enabled: true },
            MenuItem { label: "Sort by name", keys: "Ctrl+1", action: Action::SortBy(gannet_core::listing::SortKey::Name), enabled: true },
            MenuItem { label: "Sort by size", keys: "Ctrl+2", action: Action::SortBy(gannet_core::listing::SortKey::Size), enabled: true },
            MenuItem { label: "Sort by modified", keys: "Ctrl+3", action: Action::SortBy(gannet_core::listing::SortKey::Modified), enabled: true },
            MenuItem { label: "Reload", keys: "F5", action: Action::Reload, enabled: true },
        ]
    }

    /// The bar along the bottom, in place of the status line.
    pub(crate) fn paint_chooser_bar(&mut self, c: &mut Canvas) {
        let l = self.layout();
        let co = self.colors;
        c.fill(l.status, co.surface);
        c.fill(Rect::new(0, l.status.y, l.status.w, 1), co.border);
        let b = self.chooser_bar();
        if let Some(ch) = self.chooser.as_mut() {
            ch.drawn = (b.accept, b.cancel, b.filter);
        }
        let Some(ch) = self.chooser.as_ref() else { return };
        let (name, focused, accept_label) = (ch.name.clone(), ch.name_focused, ch.spec.accept_label());
        let filter_label = ch.filter.and_then(|i| ch.labels.get(i).copied());
        if let Some((r, label_x)) = b.name {
            let by = self.baseline(r.y, r.h);
            self.text.draw_str(c, "Name", label_x as f32, by, co.muted);
            self.paint_field(c, r, &name, focused);
        } else {
            // What the program asked, like "Upload files", then what's picked so far. There's
            // no title bar on many Wayland desktops, so this is where the request shows.
            let title = self.chooser.as_ref().map(|c| c.spec.title()).unwrap_or_default();
            let msg = self.chooser_info();
            let by = self.baseline(b.info.y, b.info.h);
            let (x, max) = (b.info.x as f32, b.info.w as f32);
            let title_w = self.text.measure(&title).min(max * 0.6);
            self.text.draw_fitted(c, &title, x, by, title_w, co.foreground);
            let gap = self.px(14.0) as f32;
            if x + title_w + gap < x + max {
                self.text.draw_fitted(c, &msg, x + title_w + gap, by, max - title_w - gap, co.muted);
            }
        }
        if let (Some(r), Some(label)) = (b.filter, filter_label) {
            c.rrect(r, self.px(5.0), if self.hover == Hit::ChooserFilter { co.hover } else { co.base });
            c.frame(r, 1, co.border);
            let by = self.baseline(r.y, r.h);
            let arrow = self.px(12.0);
            self.text.draw_fitted(c, label, (r.x + self.px(10.0)) as f32, by, (r.w - self.px(30.0) - arrow) as f32, co.foreground);
            self.icons.draw(c, crate::icons::Icon::ArrowDown, r.right() - self.px(10.0) - arrow, r.y + (r.h - arrow) / 2, arrow, co.muted);
        }
        let can = self.chooser_can_accept();
        for (r, label, primary, hit) in [(b.cancel, "Cancel".to_string(), false, Hit::ChooserCancel), (b.accept, accept_label, true, Hit::ChooserAccept)] {
            let hovered = self.hover == hit;
            let (bg, fg) = match (primary, can) {
                (true, true) => (if hovered { gannet_core::theme::blend(co.accent, co.foreground, 0.12) } else { co.accent }, co.on_accent),
                (true, false) => (co.hover, co.border),
                (false, _) => (if hovered { gannet_core::theme::blend(co.hover, co.foreground, 0.08) } else { co.hover }, co.foreground),
            };
            c.rrect(r, self.px(5.0), bg);
            let w = self.text.measure(&label);
            let by = self.baseline(r.y, r.h);
            self.text.draw_fitted(c, &label, r.x as f32 + ((r.w as f32 - w) / 2.0).max(self.px(8.0) as f32), by, (r.w - self.px(16.0)) as f32, fg);
        }
    }

    /// The line on the left when opening: what's picked, or what to do.
    fn chooser_info(&mut self) -> String {
        let Some(c) = self.chooser.as_ref() else { return String::new() };
        let (mode, multiple) = (c.spec.mode, c.spec.multiple);
        let Some(l) = self.listing.clone() else { return String::new() };
        let ids = self.chooser_targets();
        let files = ids.iter().filter(|&&i| !l.kind(i).is_dirlike()).count();
        let folders = ids.len() - files;
        let name = |i: u32| format!("“{}”", String::from_utf8_lossy(l.name(i)));
        match mode {
            Mode::Open if files == 1 || (files > 1 && !multiple) => name(ids.iter().copied().find(|&i| !l.kind(i).is_dirlike()).unwrap_or(0)),
            Mode::Open if files > 1 => crate::format::count(files, "file", "files"),
            Mode::Open if multiple => "Choose files to open. Ctrl or Shift picks several".into(),
            Mode::Open => "Choose a file to open".into(),
            Mode::Folder | Mode::SaveFiles if folders == 1 => name(ids.iter().copied().find(|&i| l.kind(i).is_dirlike()).unwrap_or(0)),
            Mode::Folder if folders > 1 && multiple => crate::format::count(folders, "folder", "folders"),
            Mode::SaveFiles => format!("Saves {} in “{}”", crate::format::count(c.spec.save_names.len(), "file", "files"), self.crumb_label(&self.dir)),
            _ if !self.chooser_folder_ok() => "Choose a folder".into(),
            _ => format!("“{}”", self.crumb_label(&self.dir)),
        }
    }
}

/// The name a path is shown by in the replace question.
pub fn shown_name(p: &Path) -> String {
    String::from_utf8_lossy(p.file_name().map_or(p.as_os_str().as_bytes(), |n| n.as_bytes())).into_owned()
}

/// A GTK label as shown: `_` marks the next letter as its shortcut, and `__` is an underscore.
fn without_mnemonics(label: &str) -> String {
    let mut out = String::new();
    let mut chars = label.chars();
    while let Some(c) = chars.next() {
        if c != '_' {
            out.push(c);
        } else if let Some(next) = chars.next() {
            out.push(next);
        }
    }
    out
}

/// What a chooser lets its list do, by menu or key. It can tidy up where things are saved,
/// but cutting and pasting, deleting for good or opening another window is the file manager's
/// job, not that of a dialog a program is waiting on.
pub fn allows(a: Action) -> bool {
    match a {
        Action::Open | Action::Preview | Action::ToggleHidden | Action::SortBy(_) | Action::Reload => true,
        Action::Rename | Action::NewFolder | Action::Trash | Action::Undo => true,
        Action::ShowInFolder | Action::OpenDrive | Action::Unmount | Action::Eject | Action::ChooseFilter(_) => true,
        Action::NewWindow | Action::Cut | Action::Copy | Action::Paste | Action::DeleteForGood | Action::Restore | Action::EmptyTrash => false,
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    fn os(v: &[&str]) -> Vec<OsString> {
        v.iter().map(OsString::from).collect()
    }

    #[test]
    fn a_spec_survives_the_trip_through_arguments() {
        let s = Spec {
            mode: Mode::Save,
            multiple: true,
            title: "Save “Report” – draft".into(),
            accept: Some("_Export".into()),
            folder: Some("/home/me/My Documents".into()),
            name: Some("report.final.pdf".into()),
            file: None,
            save_names: vec!["a.txt".into(), "--b".into()],
            filters: vec![Filter { label: "PDF".into(), patterns: vec![Pattern::Glob("*.pdf".into()), Pattern::Mime("application/pdf".into())] }, Filter { label: "--glob".into(), patterns: vec![] }],
            current: Some(1),
            parent: Some("wayland:abc".into()),
            modal: true,
        };
        let args = s.args();
        assert_eq!(args[0], ARG);
        assert_eq!(Spec::parse(args.into_iter().skip(1)), Some(s));
    }

    #[test]
    fn nonsense_arguments_are_refused() {
        assert_eq!(Spec::parse(os(&["sideways"])), None);
        assert_eq!(Spec::parse(os(&["open", "--title"])), None, "a flag with nothing after it");
        assert_eq!(Spec::parse(os(&["open", "--glob", "*.x"])), None, "a pattern needs a filter");
        assert_eq!(Spec::parse(os(&["open", "--what"])), None);
        assert_eq!(Spec::parse(os(&["open", "--filter", "A", "--current", "3"])).unwrap().current, None, "past the end");
    }

    #[test]
    fn titles_and_labels_have_sensible_defaults() {
        let s = |mode, multiple| Spec { mode, multiple, ..Spec::default() };
        assert_eq!(s(Mode::Open, true).title(), "Open Files");
        assert_eq!(s(Mode::Folder, false).accept_label(), "Select");
        assert_eq!(s(Mode::Save, false).accept_label(), "Save");
        // GTK's mnemonic underscores aren't shown.
        assert_eq!(Spec { accept: Some("_Upload".into()), ..Spec::default() }.accept_label(), "Upload");
        assert_eq!(Spec { accept: Some("Save as my__file".into()), ..Spec::default() }.accept_label(), "Save as my_file");
        assert_eq!(Spec { accept: Some(" ".into()), ..Spec::default() }.accept_label(), "Open");
    }

    #[test]
    fn saving_a_file_again_starts_where_it_is() {
        let d = std::env::temp_dir();
        let s = Spec { mode: Mode::Save, file: Some(d.join("notes.md")), ..Spec::default() };
        assert_eq!(s.start_folder(), Some(d.clone()));
        assert_eq!(s.start_name(), "notes.md");
        // A name given outright wins, and a folder that isn't there gives way to the file's.
        let s = Spec { name: Some("new.md".into()), folder: Some("/nonexistent/x".into()), ..s };
        assert_eq!(s.start_name(), "new.md");
        assert_eq!(s.start_folder(), Some(d.clone()));
        assert_eq!(Spec { file: None, ..s }.start_folder(), None);
    }

    #[test]
    fn filters_keep_folders_and_matching_files() {
        let f = Filter { label: "Images".into(), patterns: vec![Pattern::Glob("*.png".into()), Pattern::Glob("*.JPG".into())] };
        let r = RowFilter::new(false, Some(&f), None);
        assert!(r.keeps(b"a.png", false) && r.keeps(b"B.PNG", false) && r.keeps(b"c.jpg", false));
        assert!(!r.keeps(b"d.txt", false));
        let accented = Filter { label: "Ñ".into(), patterns: vec![Pattern::Glob("*.Ñ".into())] };
        assert!(RowFilter::new(false, Some(&accented), None).keeps("x.Ñ".as_bytes(), false), "lowered as the names are");
        assert!(r.keeps(b"folder.txt", true), "folders always show, to get around");
        let all = Filter { label: "All".into(), patterns: vec![Pattern::Glob("*".into())] };
        assert!(RowFilter::new(false, Some(&all), None).takes_all());
        assert!(RowFilter::new(false, None, None).keeps(b"x", false));
        let folders = RowFilter::new(true, None, None);
        assert!(!folders.keeps(b"x", false) && folders.keeps(b"x", true) && !folders.takes_all());
    }

    #[test]
    fn type_filters_go_by_the_file_type_database() {
        let dir = std::env::temp_dir().join(format!("gannet-test-chooser-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("globs2"), "50:image/png:*.png\n50:image/svg+xml:*.svg\n50:text/x-csrc:*.c\n").unwrap();
        std::fs::write(dir.join("subclasses"), "image/svg+xml application/xml\n").unwrap();
        let db = Arc::new(MimeDb::load(std::slice::from_ref(&dir)));
        let f = |m: &str| Filter { label: m.into(), patterns: vec![Pattern::Mime(m.into())] };
        let images = RowFilter::new(false, Some(&f("image/*")), Some(db.clone()));
        assert!(images.keeps(b"a.png", false) && images.keeps(b"b.SVG", false) && !images.keeps(b"c.c", false));
        let text = RowFilter::new(false, Some(&f("text/plain")), Some(db.clone()));
        assert!(text.keeps(b"main.c", false) && !text.keeps(b"a.png", false));
        assert!(RowFilter::new(false, Some(&f("application/xml")), Some(db)).keeps(b"x.svg", false));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_outcome_is_read_back_as_written() {
        let picked = vec![PathBuf::from("/a b/c.txt"), PathBuf::from("/d")];
        let text = outcome_text(Some(&picked), Some(2));
        assert_eq!(read_outcome(&text), Some((vec!["file:///a%20b/c.txt".to_string(), "file:///d".to_string()], Some(2))));
        assert_eq!(read_outcome(&outcome_text(None, Some(1))), None);
        assert_eq!(read_outcome(""), None, "a window that died says nothing");
        assert_eq!(read_outcome("accept\n"), None, "nothing picked is no pick");
    }
}
