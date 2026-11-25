//! Open With: a window listing the apps that can open a file, for picking one other than the
//! default.
//!
//! `gannet --open-with ...` is the window. The file list starts one for its "Open with…" and
//! opens the files with the app picked. The portal service (`portal.rs`) starts one for
//! other programs' app chooser requests and hands back the app picked, which the portal then
//! opens itself. Either way the window only says what was picked, on its standard output, as
//! the Open and Save dialogs do.

use std::collections::HashMap;
use std::ffi::OsString;
use std::io::{BufRead, Read, Write};
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use gannet_core::apps::{self, Apps};
use gannet_core::mime::MimeDb;
use gannet_core::theme::{self, Colors, Source, ThemePaths};
use rustix::event::{EventfdFlags, PollFd, PollFlags, eventfd, poll};
use rustix::fd::OwnedFd;
use xkbcommon::xkb::keysyms as k;

use crate::a11y::{self, Act, Node, Role, TextEdit};
use crate::app::{Hub, Msg};
use crate::canvas::{Canvas, Rect};
use crate::field::Field;
use crate::text::Text;
use crate::wl::{Input, Mods, Wl};

pub const ARG: &str = "--open-with";
pub const USAGE: &str = "gannet --open-with [--what TEXT] [--type MIME]... [--choices [--choice ID]...] [--last ID] [--offer-always] [--parent wayland:HANDLE] [--modal]";
/// The window's size before the compositor says otherwise, in logical pixels.
const SIZE: (i32, i32) = (520, 560);
/// Apps run in a terminal through this, which starts the user's own terminal.
const TERMINAL: &str = "xdg-terminal-exec";
/// More output than a window could mean is a broken one.
const MAX_OUTPUT: u64 = 64 * 1024;
const DOUBLE_CLICK: Duration = Duration::from_millis(400);
/// Opening a pile of files by accident is hard to undo, as with Open.
pub const MANY: usize = 20;

/// Everything a window is asked to show. It's passed to the window as arguments.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Spec {
    /// What's being opened, for the title: “photo.png”, or "3 files".
    pub what: Option<String>,
    /// The types being opened. Apps that open all of them are listed first.
    pub types: Vec<String>,
    /// Only these apps, by desktop ID, as the app chooser portal gives them. Without, every
    /// app is listed, those for the types first.
    pub choices: Option<Vec<String>>,
    /// The app to start on, as the portal remembers the last one picked.
    pub last: Option<String>,
    /// Offers "Always use it", which makes the app picked the default for the type.
    pub offer_always: bool,
    /// The window to sit above, as `wayland:HANDLE`.
    pub parent: Option<String>,
    pub modal: bool,
}

impl Spec {
    /// The arguments that make a window show this. Every value is its own argument, so nothing
    /// needs quoting.
    pub fn args(&self) -> Vec<OsString> {
        let mut a: Vec<OsString> = vec![ARG.into()];
        let mut pair = |flag: &str, v: &str| {
            a.push(flag.into());
            a.push(v.into());
        };
        if let Some(w) = &self.what {
            pair("--what", w);
        }
        for t in &self.types {
            pair("--type", t);
        }
        if let Some(l) = &self.last {
            pair("--last", l);
        }
        if let Some(p) = &self.parent {
            pair("--parent", p);
        }
        if let Some(c) = &self.choices {
            a.push("--choices".into());
            for id in c {
                a.push("--choice".into());
                a.push(id.into());
            }
        }
        if self.offer_always {
            a.push("--offer-always".into());
        }
        if self.modal {
            a.push("--modal".into());
        }
        a
    }

    /// Reads the arguments after `--open-with`. `None` if they don't make sense.
    pub fn parse(args: impl IntoIterator<Item = OsString>) -> Option<Self> {
        let mut it = args.into_iter();
        let mut s = Spec::default();
        let text = |v: Option<OsString>| v.map(|v| v.to_string_lossy().into_owned());
        while let Some(flag) = it.next() {
            match flag.to_str()? {
                "--what" => s.what = Some(text(it.next())?),
                "--type" => s.types.push(text(it.next())?),
                "--last" => s.last = Some(text(it.next())?),
                "--parent" => s.parent = Some(text(it.next())?),
                "--choices" => s.choices = Some(Vec::new()),
                "--choice" => s.choices.as_mut()?.push(text(it.next())?),
                "--offer-always" => s.offer_always = true,
                "--modal" => s.modal = true,
                _ => return None,
            }
        }
        Some(s)
    }

    pub fn title(&self) -> String {
        match &self.what {
            Some(w) => format!("Open {w} with"),
            None => "Open with".to_string(),
        }
    }
}

/// What the window says when it closes: `open` and the app's ID, then `always` if it's to be
/// the default; or `cancel`.
pub fn outcome_text(picked: Option<(&str, bool)>) -> String {
    match picked {
        Some((id, always)) => format!("open {id}\n{}", if always { "always\n" } else { "" }),
        None => "cancel\n".to_string(),
    }
}

/// Reads what the window said: the app and whether it's to be the default, or `None`.
pub fn read_outcome(text: &str) -> Option<(String, bool)> {
    let mut lines = text.lines();
    let id = lines.next()?.strip_prefix("open ")?.to_string();
    (!id.is_empty()).then(|| (id, lines.next() == Some("always")))
}

/// A new list of apps for an open window, sent on its standard input.
pub fn choices_line(ids: &[String]) -> String {
    std::iter::once("choices").chain(ids.iter().map(String::as_str)).collect::<Vec<_>>().join("\t")
}

fn read_choices_line(line: &str) -> Option<Vec<String>> {
    let mut parts = line.split('\t');
    (parts.next()? == "choices").then(|| parts.filter(|p| !p.is_empty()).map(str::to_string).collect())
}

/// One app in the list.
#[derive(Clone, Debug, PartialEq)]
pub struct Choice {
    /// As it's answered with: the desktop ID, or the portal's name for it.
    pub id: String,
    pub name: String,
    pub default: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Row {
    Heading(String),
    App(Choice),
}

/// How the window closes.
#[derive(Debug, PartialEq)]
pub enum Finish {
    Open(String),
    Cancel,
}

/// The window's list and what's been typed and chosen, apart from drawing it.
pub struct Picker {
    pub spec: Spec,
    /// The apps for the types, or the ones the program offered.
    suggested: Vec<Choice>,
    /// Every other app, when it's not the program's own list.
    others: Vec<Choice>,
    /// The types' name for people, like "PNG image", for the heading and "Always use it".
    pub kind: Option<String>,
    pub search: Field,
    pub rows: Vec<Row>,
    /// The row picked, always an app.
    pub cursor: Option<usize>,
    pub always: bool,
}

impl Picker {
    pub fn new(spec: Spec, apps: &Apps, db: Option<&MimeDb>, lang: Option<&str>) -> Self {
        let kind = match spec.types.as_slice() {
            [one] => db.and_then(|d| d.describe(one, lang)),
            _ => None,
        };
        let mut p = Picker { spec, suggested: Vec::new(), others: Vec::new(), kind, search: Field::default(), rows: Vec::new(), cursor: None, always: false };
        p.fill(apps, db);
        let last = p.spec.last.clone();
        p.rebuild();
        if let Some(i) = last.and_then(|l| p.rows.iter().position(|r| matches!(r, Row::App(c) if c.id == l))) {
            p.cursor = Some(i);
        }
        p
    }

    /// Fills the lists from what's installed.
    fn fill(&mut self, apps: &Apps, db: Option<&MimeDb>) {
        if let Some(ids) = &self.spec.choices {
            // The program's own list, in its order, of apps that are installed.
            let mut seen = Vec::new();
            self.suggested = ids.iter().filter_map(|id| {
                let app = apps.find(id)?;
                (!seen.contains(&app.id)).then(|| {
                    seen.push(app.id.clone());
                    Choice { id: id.clone(), name: app.name.clone(), default: false }
                })
            }).collect();
            return;
        }
        let ancestors: Vec<Vec<String>> = self.spec.types.iter().map(|t| db.map_or_else(|| vec![t.clone()], |d| d.ancestors(t))).collect();
        let default = match ancestors.as_slice() {
            [one] => apps.named_default(one).map(|a| a.id.clone()),
            _ => None,
        };
        let choice = |a: &apps::App| Choice { id: a.id.clone(), name: a.name.clone(), default: default.as_deref() == Some(a.id.as_str()) };
        // An app with nowhere in its command line for a file can't open one.
        self.suggested = apps.for_all(&ancestors).into_iter().filter(|a| a.takes_files()).map(choice).collect();
        self.others = apps.all_shown().into_iter().filter(|a| a.takes_files() && !self.suggested.iter().any(|s| s.id == a.id)).map(choice).collect();
    }

    /// The program found other apps while the window was open.
    pub fn set_choices(&mut self, ids: Vec<String>, apps: &Apps) {
        let keep = self.picked().map(|c| c.id.clone());
        self.spec.choices = Some(ids);
        self.fill(apps, None);
        self.rebuild();
        if let Some(i) = keep.and_then(|id| self.rows.iter().position(|r| matches!(r, Row::App(c) if c.id == id))) {
            self.cursor = Some(i);
        }
    }

    pub fn picked(&self) -> Option<&Choice> {
        match self.rows.get(self.cursor?) {
            Some(Row::App(c)) => Some(c),
            _ => None,
        }
    }

    /// The rows for what's typed: headings, then the apps whose names or IDs have it in them.
    /// The cursor stays on its app if it's still there, or goes to the first.
    pub fn rebuild(&mut self) {
        let keep = self.picked().map(|c| c.id.clone());
        let needle = self.search.text.trim().to_lowercase();
        let matches = |c: &&Choice| needle.is_empty() || c.name.to_lowercase().contains(&needle) || c.id.to_lowercase().contains(&needle);
        let suggested: Vec<Choice> = self.suggested.iter().filter(matches).cloned().collect();
        let others: Vec<Choice> = self.others.iter().filter(matches).cloned().collect();
        self.rows.clear();
        if self.spec.choices.is_some() {
            if let Some(k) = self.kind.as_ref().filter(|_| !suggested.is_empty()) {
                self.rows.push(Row::Heading(format!("For {k}")));
            }
            self.rows.extend(suggested.into_iter().map(Row::App));
        } else {
            if !suggested.is_empty() {
                let heading = match &self.kind {
                    Some(k) => format!("For {k}"),
                    None if self.spec.types.len() > 1 => "For all of these".to_string(),
                    None => "For this type".to_string(),
                };
                self.rows.push(Row::Heading(heading));
                self.rows.extend(suggested.into_iter().map(Row::App));
            }
            if !others.is_empty() {
                self.rows.push(Row::Heading("Other apps".to_string()));
                self.rows.extend(others.into_iter().map(Row::App));
            }
        }
        let first = self.rows.iter().position(|r| matches!(r, Row::App(_)));
        self.cursor = keep.and_then(|id| self.rows.iter().position(|r| matches!(r, Row::App(c) if c.id == id))).or(first);
    }

    /// Moves the cursor `by` apps, stopping at the ends.
    pub fn step(&mut self, by: isize) {
        let apps: Vec<usize> = self.rows.iter().enumerate().filter(|(_, r)| matches!(r, Row::App(_))).map(|(i, _)| i).collect();
        let Some(&last) = apps.last() else { return };
        let at = self.cursor.and_then(|c| apps.iter().position(|&i| i == c)).unwrap_or(0) as isize;
        let to = (at + by).clamp(0, apps.len() as isize - 1) as usize;
        self.cursor = Some(apps.get(to).copied().unwrap_or(last));
    }

    /// Whether "Always use it" applies: offered, and not for another program's request.
    pub fn can_always(&self) -> bool {
        self.spec.offer_always && self.spec.choices.is_none()
    }

    pub fn always_label(&self) -> String {
        match &self.kind {
            Some(k) => format!("Always use for {k} files"),
            None => "Always use for this type of file".to_string(),
        }
    }

    /// A key, with `page` apps to a page. Returns how the window closes, if it does.
    pub fn key(&mut self, sym: u32, utf8: &str, m: Mods, page: usize) -> Option<Finish> {
        let word = m.ctrl;
        match sym {
            k::KEY_Escape if !self.search.text.is_empty() => {
                self.search = Field::default();
                self.rebuild();
            }
            k::KEY_Escape => return Some(Finish::Cancel),
            k::KEY_Return | k::KEY_KP_Enter => return self.picked().map(|c| Finish::Open(c.id.clone())),
            k::KEY_Up => self.step(-1),
            k::KEY_Down => self.step(1),
            k::KEY_Page_Up => self.step(-(page.max(1) as isize)),
            k::KEY_Page_Down => self.step(page.max(1) as isize),
            k::KEY_Home => self.step(isize::MIN / 2),
            k::KEY_End => self.step(isize::MAX / 2),
            k::KEY_Left => self.search.left(word, m.shift),
            k::KEY_Right => self.search.right(word, m.shift),
            k::KEY_BackSpace => {
                self.search.backspace(word);
                self.rebuild();
            }
            k::KEY_Delete => {
                self.search.delete(word);
                self.rebuild();
            }
            // Space ticks "Always use it" until there's a search, where it's a space.
            k::KEY_space if self.search.text.is_empty() && self.can_always() && !m.ctrl && !m.alt => self.always = !self.always,
            _ if !utf8.is_empty() && !m.ctrl && !m.alt && !m.logo && !utf8.chars().any(char::is_control) => {
                self.search.insert(utf8);
                self.rebuild();
            }
            _ => {}
        }
        None
    }
}

/// The parts of the window, in physical pixels.
struct Layout {
    title: Rect,
    search: Rect,
    list: Rect,
    bar: Rect,
    always: Option<Rect>,
    cancel: Rect,
    open: Rect,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Hit {
    None,
    Row(usize),
    Always,
    Cancel,
    Open,
}

/// The window: the picker, drawn, and the pointer.
struct Window {
    picker: Picker,
    apps: Apps,
    text: Text,
    colors: Colors,
    scale: f64,
    w: i32,
    h: i32,
    scroll: i32,
    pointer: (i32, i32),
    hover: Hit,
    last_click: Option<(Instant, usize)>,
    dirty: bool,
    finish: Option<Finish>,
    has_keyboard: bool,
    /// Each app's and heading's number for screen readers, kept while the window's open, so
    /// one keeps its object as a search comes and goes.
    a11y_ids: HashMap<String, u64>,
}

impl Window {
    fn px(&self, v: f64) -> i32 {
        (v * self.scale).round() as i32
    }

    fn baseline(&self, top: i32, h: i32) -> i32 {
        top + ((h as f32 + self.text.ascent - self.text.descent) / 2.0).round() as i32
    }

    fn row_h(&self, r: &Row) -> i32 {
        match r {
            Row::Heading(_) => self.px(28.0),
            Row::App(_) => self.px(30.0),
        }
    }

    fn layout(&mut self) -> Layout {
        let pad = self.px(16.0);
        let title = Rect::new(pad, self.px(12.0), self.w - 2 * pad, self.px(26.0));
        let search = Rect::new(pad, title.bottom() + self.px(8.0), self.w - 2 * pad, self.px(32.0));
        // "Always use it" has a line of its own above the buttons, as the type's name can be long.
        let offer = self.picker.can_always();
        let bar_h = self.px(if offer { 96.0 } else { 60.0 });
        let bar = Rect::new(0, self.h - bar_h, self.w, bar_h);
        let list_top = search.bottom() + self.px(10.0);
        let list = Rect::new(0, list_top, self.w, (bar.y - list_top).max(0));
        let h = self.px(32.0);
        let y = bar.bottom() - self.px(14.0) - h;
        let button_w = |s: &mut Self, label: &str| (s.text.measure(label) as i32 + s.px(36.0)).max(s.px(92.0));
        let w = button_w(self, "Open");
        let open = Rect::new(self.w - pad - w, y, w, h);
        let w = button_w(self, "Cancel");
        let cancel = Rect::new(open.x - self.px(8.0) - w, y, w, h);
        // Only the box and its label take a click.
        let label_w = self.text.measure(&self.picker.always_label()) as i32 + self.px(22.0);
        let always = offer.then(|| Rect::new(pad, bar.y + self.px(10.0), label_w.min(self.w - 2 * pad), self.px(28.0)));
        Layout { title, search, list, bar, always, cancel, open }
    }

    /// Each row's top, from the list's top, and the height of them all.
    fn row_tops(&self) -> (Vec<i32>, i32) {
        let mut y = 0;
        let tops = self.picker.rows.iter().map(|r| {
            let top = y;
            y += self.row_h(r);
            top
        }).collect();
        (tops, y)
    }

    /// Scrolls so the cursor's row is in view.
    fn reveal(&mut self) {
        let list = self.layout().list;
        let (tops, total) = self.row_tops();
        if let Some(c) = self.picker.cursor {
            let (top, h) = (tops[c], self.row_h(&self.picker.rows[c]));
            // The heading above the first app shows too.
            let top = if self.picker.rows[..c].iter().all(|r| matches!(r, Row::Heading(_))) { 0 } else { top };
            if top < self.scroll {
                self.scroll = top;
            } else if top + h > self.scroll + list.h {
                self.scroll = top + h - list.h;
            }
        }
        self.scroll = self.scroll.clamp(0, (total - list.h).max(0));
    }

    fn page(&mut self) -> usize {
        (self.layout().list.h / self.px(30.0).max(1)).max(1) as usize
    }

    fn hit(&mut self, x: i32, y: i32) -> Hit {
        let l = self.layout();
        if l.list.contains(x, y) {
            let (tops, _) = self.row_tops();
            let at = y - l.list.y + self.scroll;
            return match tops.iter().rposition(|&t| t <= at) {
                Some(i) if matches!(self.picker.rows[i], Row::App(_)) && at < tops[i] + self.row_h(&self.picker.rows[i]) => Hit::Row(i),
                _ => Hit::None,
            };
        }
        if l.open.contains(x, y) && self.picker.picked().is_some() {
            Hit::Open
        } else if l.cancel.contains(x, y) {
            Hit::Cancel
        } else if l.always.is_some_and(|r| r.contains(x, y)) {
            Hit::Always
        } else {
            Hit::None
        }
    }

    fn input(&mut self, i: Input) {
        match i {
            // A held key that cleared the search, or ticked the box, doesn't go on to cancel,
            // open, or tick it back.
            Input::Key { keysym: k::KEY_Escape | k::KEY_Return | k::KEY_KP_Enter | k::KEY_space, repeat: true, .. } => {}
            Input::Key { keysym, utf8, mods, .. } => {
                let page = self.page();
                // A click that already closed it isn't undone by a key in the same batch.
                if let Some(f) = self.picker.key(keysym, &utf8, mods, page) {
                    self.finish = Some(f);
                }
                self.reveal();
                self.dirty = true;
            }
            Input::Move { x, y } => {
                self.pointer = ((x * self.scale) as i32, (y * self.scale) as i32);
                let hit = self.hit(self.pointer.0, self.pointer.1);
                if hit != self.hover {
                    self.hover = hit;
                    self.dirty = true;
                }
            }
            Input::Leave => {
                self.hover = Hit::None;
                self.dirty = true;
            }
            Input::Button { button: 0x110, pressed: true } => {
                match self.hit(self.pointer.0, self.pointer.1) {
                    Hit::Row(i) => {
                        let now = Instant::now();
                        let double = self.last_click.is_some_and(|(t, r)| r == i && now.duration_since(t) < DOUBLE_CLICK);
                        self.picker.cursor = Some(i);
                        self.last_click = Some((now, i));
                        if double {
                            self.finish = self.picker.picked().map(|c| Finish::Open(c.id.clone()));
                        }
                    }
                    Hit::Open => self.finish = self.picker.picked().map(|c| Finish::Open(c.id.clone())),
                    Hit::Cancel => self.finish = Some(Finish::Cancel),
                    Hit::Always => self.picker.always = !self.picker.always,
                    Hit::None => {}
                }
                self.dirty = true;
            }
            Input::Scroll { lines, pixels } => {
                self.scroll += (lines * 3.0 * self.px(30.0) as f64 + pixels * self.scale) as i32;
                let list = self.layout().list;
                let (_, total) = self.row_tops();
                self.scroll = self.scroll.clamp(0, (total - list.h).max(0));
                self.dirty = true;
            }
            Input::Resized => self.dirty = true,
            Input::Close => self.finish = Some(Finish::Cancel),
            Input::Focus(on) => self.has_keyboard = on,
            _ => {}
        }
    }

    fn resized(&mut self, w: i32, h: i32, scale: f64) {
        self.w = w;
        self.h = h;
        self.scale = scale;
        self.text.set_size(crate::text::UI_PX * scale as f32, crate::text::UI_HINTED && scale < 1.5);
        self.reveal();
        self.dirty = true;
    }

    /// A row's number for screen readers.
    fn a11y_id(&mut self, row: &Row) -> u64 {
        let key = match row {
            Row::Heading(h) => format!("# {h}"),
            Row::App(c) => c.id.clone(),
        };
        let next = a11y::OPEN_WITH_ROW0 + self.a11y_ids.len() as u64;
        *self.a11y_ids.entry(key).or_insert(next)
    }

    /// What screen readers see: the search box, the apps under their headings with the
    /// default marked, and the buttons. The app picked has the keyboard, as its row is where
    /// the arrows move.
    fn a11y_snapshot(&mut self, rows: a11y::Rows, prev: Option<&a11y::TextGeometry>) -> a11y::Snapshot {
        let l = self.layout();
        let (tops, _) = self.row_tops();
        let full = Rect::new(0, 0, self.w, self.h);
        let mut fixed = vec![Node::new(a11y::FRAME, Role::Dialog, self.picker.spec.title(), full, 0)];
        let mut search = Node::new(a11y::OPEN_WITH_SEARCH, Role::Entry, "Search apps", l.search, a11y::FRAME);
        search.field = Some(self.picker.search.clone());
        let mut items = Vec::new();
        let mut focus = a11y::Focus::Node(a11y::OPEN_WITH_SEARCH);
        for (i, row) in self.picker.rows.clone().iter().enumerate() {
            let id = self.a11y_id(row);
            let r = Rect::new(l.list.x, l.list.y + tops[i] - self.scroll, l.list.w, self.row_h(row));
            let showing = r.y + r.h > l.list.y && r.y < l.list.bottom();
            items.push(match row {
                Row::Heading(h) => Node::new(id, Role::Heading, h.clone(), r, a11y::OPEN_WITH_APPS).showing(showing),
                Row::App(c) => {
                    if self.picker.cursor == Some(i) {
                        focus = a11y::Focus::Node(id);
                    }
                    Node::new(id, Role::ListItem, c.name.clone(), r, a11y::OPEN_WITH_APPS).described(if c.default { "Default" } else { "" }).selected(self.picker.cursor == Some(i)).showing(showing).doing(a11y::Action::new("activate", "Open with this app", "Return"))
                }
            });
        }
        let apps = Node::new(a11y::OPEN_WITH_APPS, Role::List, "Apps", l.list, a11y::FRAME).with_children(items.iter().map(|n| n.id).collect());
        fixed[0].children = vec![a11y::OPEN_WITH_SEARCH, a11y::OPEN_WITH_APPS];
        fixed.extend([search, apps]);
        fixed.extend(items);
        if let Some(r) = l.always {
            // Space ticks it only until there's a search, where it's typed.
            let keys = if self.picker.search.text.is_empty() { "space" } else { "" };
            let mut always = Node::new(a11y::OPEN_WITH_ALWAYS, Role::CheckBox, self.picker.always_label(), r, a11y::FRAME).doing(a11y::Action::new("toggle", "Make it the default", keys));
            always.checked = Some(self.picker.always);
            fixed[0].children.push(always.id);
            fixed.push(always);
        }
        let can = self.picker.picked().is_some();
        fixed[0].children.extend([a11y::OPEN_WITH_CANCEL, a11y::OPEN_WITH_OPEN]);
        fixed.push(Node::new(a11y::OPEN_WITH_CANCEL, Role::PushButton, "Cancel", l.cancel, a11y::FRAME).doing(a11y::click("Close without opening", "Escape")));
        fixed.push(Node::new(a11y::OPEN_WITH_OPEN, Role::PushButton, "Open", l.open, a11y::FRAME).enabled(can).doing(a11y::click("Open with the app picked", "Return")));
        // Where the search's characters are, as `paint_search` places them.
        let f = &self.picker.search;
        let (r, line_h) = (l.search, self.text.line_height() as i32);
        let shift = (self.text.measure(&f.text[..f.caret]) - (r.w - self.px(16.0)) as f32 + 2.0).max(0.0);
        let x0 = (r.x + self.px(8.0)) as f32 - shift;
        let line = Rect::new(r.x, r.y + (r.h - line_h) / 2, r.w, line_h);
        let geometry = a11y::TextGeometry::measure(&mut self.text, f, x0, line, prev);
        a11y::Snapshot { fixed, rows, focus, window_focused: self.has_keyboard, dir: Arc::default(), text_geometry: vec![(a11y::OPEN_WITH_SEARCH, Arc::new(geometry))], edit_gen: 0, news: a11y::News::default(), modal: None }
    }

    /// Does what a screen reader asked, as the pointer would.
    fn a11y_act(&mut self, act: Act) {
        let row = |w: &mut Self, id: u64| (0..w.picker.rows.len()).find(|&i| matches!(w.picker.rows[i], Row::App(_)) && w.a11y_id(&w.picker.rows[i].clone()) == id);
        match act {
            Act::Press(a11y::OPEN_WITH_OPEN) => self.finish = self.picker.picked().map(|c| Finish::Open(c.id.clone())),
            Act::Press(a11y::OPEN_WITH_CANCEL) => self.finish = Some(Finish::Cancel),
            Act::Press(a11y::OPEN_WITH_ALWAYS) if self.picker.can_always() => self.picker.always = !self.picker.always,
            Act::Press(id) => {
                if let Some(i) = row(self, id) {
                    self.picker.cursor = Some(i);
                    self.finish = self.picker.picked().map(|c| Finish::Open(c.id.clone()));
                }
            }
            Act::Focus(id) => {
                if let Some(i) = row(self, id) {
                    self.picker.cursor = Some(i);
                    self.reveal();
                }
            }
            // Typing in the search box through a screen reader, as on the keyboard: the apps
            // shown follow the text. The window has no clipboard, as Ctrl+C, X and V do nothing
            // here, so copying and pasting don't either, and cutting is refused.
            Act::EditText(a11y::OPEN_WITH_SEARCH, _, op) => {
                let s = &mut self.picker.search;
                let before = s.text.clone();
                match op {
                    TextEdit::Caret(at) => s.select_chars(at, at),
                    TextEdit::Select(from, to) => s.select_chars(from, to),
                    TextEdit::Replace(from, to, with) => s.replace_chars(from, to, &with),
                    TextEdit::Copy(..) | TextEdit::Cut(..) | TextEdit::Paste(_) => return,
                }
                if self.picker.search.text != before {
                    self.picker.rebuild();
                    self.reveal();
                }
            }
            _ => return,
        }
        self.dirty = true;
    }

    fn paint(&mut self, c: &mut Canvas) {
        let co = self.colors;
        let l = self.layout();
        c.fill(Rect::new(0, 0, self.w, self.h), co.base);
        let title = self.picker.spec.title();
        let by = self.baseline(l.title.y, l.title.h);
        self.text.draw_fitted(c, &title, l.title.x as f32, by, l.title.w as f32, co.foreground);
        self.paint_search(c, l.search);
        self.paint_list(c, l.list);
        self.paint_bar(c, &l);
        // An edge of its own, as it often floats over a window of the same colors.
        c.frame(Rect::new(0, 0, self.w, self.h), 1, co.border);
    }

    fn paint_search(&mut self, c: &mut Canvas, r: Rect) {
        let co = self.colors;
        c.rrect(r, self.px(5.0), co.surface);
        c.frame(r, 1, co.accent);
        let inner = Rect::new(r.x + self.px(8.0), r.y, r.w - self.px(16.0), r.h);
        let y = self.baseline(r.y, r.h);
        let f = self.picker.search.clone();
        let line = self.text.line_height() as i32;
        let line_y = r.y + (r.h - line) / 2;
        let caret_x = self.text.measure(&f.text[..f.caret]);
        let shift = (caret_x - inner.w as f32 + 2.0).max(0.0);
        let x0 = inner.x as f32 - shift;
        let (a, b) = f.selection();
        let (sa, sb) = (self.text.measure(&f.text[..a]), self.text.measure(&f.text[..b]));
        let old = c.clip();
        c.set_clip(old.intersect(inner));
        if a != b {
            c.fill(Rect::new((x0 + sa) as i32, line_y, (sb - sa) as i32, line), co.selection);
        }
        if f.text.is_empty() {
            self.text.draw_str(c, "Type to search apps", x0, y, co.muted);
        } else {
            self.text.draw_str(c, &f.text, x0, y, co.foreground);
        }
        c.fill(Rect::new((x0 + caret_x) as i32, line_y, 1, line), co.foreground);
        c.set_clip(old);
    }

    fn paint_list(&mut self, c: &mut Canvas, list: Rect) {
        let co = self.colors;
        let old = c.clip();
        c.set_clip(old.intersect(list));
        let (tops, _) = self.row_tops();
        let pad = self.px(16.0);
        let rows = self.picker.rows.clone();
        for (i, row) in rows.iter().enumerate() {
            let h = self.row_h(row);
            let y = list.y + tops[i] - self.scroll;
            if y + h < list.y || y > list.bottom() {
                continue;
            }
            match row {
                Row::Heading(text) => {
                    let by = self.baseline(y + self.px(4.0), h - self.px(4.0));
                    self.text.draw_fitted(c, text, pad as f32, by, (list.w - 2 * pad) as f32, co.muted);
                }
                Row::App(choice) => {
                    let r = Rect::new(self.px(8.0), y, list.w - self.px(16.0), h);
                    let picked = self.picker.cursor == Some(i);
                    if picked {
                        c.rrect(r, self.px(5.0), co.selection);
                    } else if self.hover == Hit::Row(i) {
                        c.rrect(r, self.px(5.0), co.hover);
                    }
                    let fg = if picked { co.on_selection } else { co.foreground };
                    let by = self.baseline(y, h);
                    let note_w = if choice.default { self.text.measure("Default") as i32 + self.px(12.0) } else { 0 };
                    self.text.draw_fitted(c, &choice.name, pad as f32, by, (list.w - 2 * pad - note_w) as f32, fg);
                    if choice.default {
                        let w = self.text.measure("Default");
                        self.text.draw_str(c, "Default", (list.w - pad) as f32 - w, by, if picked { co.on_selection } else { co.muted });
                    }
                }
            }
        }
        if self.picker.picked().is_none() {
            let q = self.picker.search.text.trim().to_string();
            let msg = if !q.is_empty() {
                format!("No app matches “{q}”")
            } else if self.picker.spec.choices.is_some() {
                "There are no apps to choose from".to_string()
            } else {
                "No apps are installed".to_string()
            };
            let w = self.text.measure(&msg).min((list.w - 2 * pad) as f32);
            let by = self.baseline(list.y + self.px(24.0), self.px(30.0));
            self.text.draw_fitted(c, &msg, (list.w as f32 - w) / 2.0, by, w, co.muted);
        }
        c.set_clip(old);
    }

    fn paint_bar(&mut self, c: &mut Canvas, l: &Layout) {
        let co = self.colors;
        c.fill(l.bar, co.surface);
        c.fill(Rect::new(0, l.bar.y, l.bar.w, 1), co.border);
        if let Some(r) = l.always {
            let s = self.px(14.0);
            let bx = Rect::new(r.x, r.y + (r.h - s) / 2, s, s);
            let on = self.picker.always;
            if on {
                c.rrect(bx, self.px(3.0), co.accent);
            }
            c.frame(bx, 1, if on || self.hover == Hit::Always { co.accent } else { co.muted });
            let label = self.picker.always_label();
            let by = self.baseline(r.y, r.h);
            let x = r.x + s + self.px(8.0);
            self.text.draw_fitted(c, &label, x as f32, by, (l.bar.right() - self.px(16.0) - x) as f32, co.foreground);
        }
        let can = self.picker.picked().is_some();
        for (r, label, primary, hit) in [(l.cancel, "Cancel", false, Hit::Cancel), (l.open, "Open", true, Hit::Open)] {
            let hovered = self.hover == hit;
            let (bg, fg) = match (primary, can) {
                (true, true) => (if hovered { theme::blend(co.accent, co.foreground, 0.12) } else { co.accent }, co.on_accent),
                (true, false) => (co.hover, co.border),
                (false, _) => (if hovered { theme::blend(co.hover, co.foreground, 0.08) } else { co.hover }, co.foreground),
            };
            c.rrect(r, self.px(5.0), bg);
            let w = self.text.measure(label);
            let by = self.baseline(r.y, r.h);
            self.text.draw_str(c, label, r.x as f32 + (r.w as f32 - w) / 2.0, by, fg);
        }
    }
}

/// `gannet --open-with ...`: the window, on its own. Never returns.
pub fn main() -> ! {
    let Some(spec) = Spec::parse(std::env::args_os().skip(2)) else {
        eprintln!("usage: {USAGE}");
        std::process::exit(2);
    };
    let out = crate::chooser::take_stdout();
    let d = crate::dirs();
    let lang = crate::language();
    let (colors, source) = theme::load(&ThemePaths::for_home(&d.home, &d.config));
    // What takes a moment happens while the window opens.
    let desktop_dark = (source != Source::AliasOs).then(|| std::thread::spawn(theme::query_gsettings_dark));
    let (mime_dirs, app_dirs, lists, desktops, programs, wanted) = (d.mime.clone(), d.apps.clone(), d.mimeapps.clone(), d.desktops.clone(), d.programs.clone(), lang.clone());
    let need_db = !spec.types.is_empty();
    let loading = std::thread::spawn(move || {
        let db = need_db.then(|| MimeDb::load(&mime_dirs));
        (db, Apps::load(&app_dirs, &lists, &desktops, wanted.as_deref(), &programs))
    });
    let parent = spec.parent.as_deref().and_then(|p| p.strip_prefix("wayland:")).map(str::to_string);
    let mut wl = match Wl::connect_dialog(&spec.title(), crate::chooser::APP_ID, SIZE.0, SIZE.1, parent.as_deref(), spec.modal) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("gannet: {e}");
            std::process::exit(1);
        }
    };
    let colors = match desktop_dark.and_then(|t| t.join().ok()).flatten() {
        Some(dark) => theme::builtin(dark),
        None => colors,
    };
    let mut text = Text::new(crate::text::UI_PX, crate::text::UI_HINTED);
    text.set_contrast(colors.dark);
    match crate::text::primary_font(&d.cache) {
        Some((path, index)) => {
            text.add_font(&path, index);
        }
        None => eprintln!("gannet: no font found; is fontconfig installed?"),
    }
    let (db, apps) = loading.join().unwrap_or_default();
    let picker = Picker::new(spec, &apps, db.as_ref(), lang.as_deref());
    let (w, h) = wl.physical_size();
    let mut win = Window { picker, apps, text, colors, scale: 1.0, w, h, scroll: 0, pointer: (0, 0), hover: Hit::None, last_click: None, dirty: true, finish: None, has_keyboard: false, a11y_ids: HashMap::new() };
    win.resized(w, h, wl.scale());
    win.reveal();

    // A new list of apps, from the service.
    let wake = Arc::new(eventfd(0, EventfdFlags::CLOEXEC | EventfdFlags::NONBLOCK).expect("eventfd"));
    let updates = listen_for_choices(wake.clone());
    // What a screen reader asks for, while one is running.
    let (tx, asked) = channel();
    let hub = Hub { tx, wake: wake.clone() };
    let mut a11y = a11y::Link::new();
    let mut first_frame = true;
    #[cfg(feature = "bench")]
    let mut bench = bench::Bench::new();
    loop {
        if let Err(e) = wl.queue.dispatch_pending(&mut wl.state) {
            eprintln!("gannet: Wayland error: {e}");
            std::process::exit(1);
        }
        while let Some(i) = wl.state.inputs.pop_front() {
            if matches!(i, Input::Resized) {
                let (w, h) = wl.physical_size();
                win.resized(w, h, wl.scale());
            }
            win.input(i);
        }
        while let Ok(m) = asked.try_recv() {
            // A click that already closed it isn't undone.
            if let Msg::A11y(act) = m
                && win.finish.is_none()
            {
                win.a11y_act(act);
            }
        }
        while let Ok(ids) = updates.try_recv() {
            let apps = std::mem::take(&mut win.apps);
            win.picker.set_choices(ids, &apps);
            win.apps = apps;
            win.reveal();
            win.dirty = true;
        }
        if let Some(f) = win.finish.take() {
            let said = match f {
                Finish::Open(id) => outcome_text(Some((&id, win.picker.always && win.picker.can_always()))),
                Finish::Cancel => outcome_text(None),
            };
            if let Some(fd) = out {
                let _ = std::fs::File::from(fd).write_all(said.as_bytes());
            }
            std::process::exit(0);
        }
        if a11y.listening() {
            let snap = win.a11y_snapshot(a11y.no_rows(), a11y.geometry(a11y::OPEN_WITH_SEARCH));
            a11y.offer(snap);
        }
        if wl.must_paint() || (win.dirty && wl.can_paint()) {
            win.dirty = false;
            wl.paint(Rect::new(0, 0, i32::MAX / 2, i32::MAX / 2), |c| win.paint(c));
            // Served to screen readers once it's up, as the other windows are.
            if std::mem::take(&mut first_frame) {
                let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
                if let Some(address) = gannet_core::bus::session_address(std::env::var("DBUS_SESSION_BUS_ADDRESS").ok().as_deref(), runtime.as_deref()) {
                    crate::session::watch(hub.clone(), address, false, a11y.shared.clone(), std::env::var("AT_SPI_BUS_ADDRESS").ok());
                }
            }
        }
        #[cfg(feature = "bench")]
        bench.idle(&win);
        if let Err(e) = wl.conn.flush()
            && !matches!(&e, wayland_client::backend::WaylandError::Io(io) if io.kind() == std::io::ErrorKind::WouldBlock)
        {
            eprintln!("gannet: lost the Wayland connection: {e}");
            std::process::exit(1);
        }
        let Some(guard) = wl.queue.prepare_read() else { continue };
        let mut fds = vec![PollFd::from_borrowed_fd(wl.fd(), PollFlags::IN), PollFd::new(&*wake, PollFlags::IN)];
        if let Some(f) = wl.repeat_fd() {
            fds.push(PollFd::from_borrowed_fd(f, PollFlags::IN));
        }
        let _ = poll(&mut fds, None);
        let ready = |i: usize| fds.get(i).is_some_and(|f| f.revents().intersects(PollFlags::IN | PollFlags::ERR | PollFlags::HUP));
        let (wl_ready, wake_ready, repeat_ready) = (ready(0), ready(1), ready(2));
        let hung_up = fds[0].revents().intersects(PollFlags::HUP | PollFlags::ERR);
        drop(fds);
        if wl_ready && let Err(e) = guard.read() && !matches!(&e, wayland_client::backend::WaylandError::Io(io) if io.kind() == std::io::ErrorKind::WouldBlock) {
            eprintln!("gannet: reading from the compositor failed: {e}");
            std::process::exit(1);
        }
        if hung_up {
            std::process::exit(1);
        }
        if wake_ready {
            let mut buf = [0u8; 8];
            let _ = rustix::io::read(&*wake, &mut buf);
        }
        if repeat_ready {
            wl.on_repeat_timer();
        }
    }
}

/// Reads new lists of apps from standard input, as the service sends them for UpdateChoices.
fn listen_for_choices(wake: Arc<OwnedFd>) -> Receiver<Vec<String>> {
    let (tx, rx) = channel();
    let _ = std::thread::Builder::new().name("gannet-choices".into()).spawn(move || {
        for line in std::io::stdin().lock().lines().map_while(Result::ok) {
            if let Some(ids) = read_choices_line(&line) {
                if tx.send(ids).is_err() {
                    return;
                }
                let _ = rustix::io::write(wake.as_fd(), &1u64.to_ne_bytes());
            }
        }
    });
    rx
}

/// "Open with…" in the file list: asks which app in a window over this one, then opens the
/// files with it, and makes it the default if that was ticked. Works on a thread of its own,
/// so nothing waits on the window. `parent` is the list's window handle, if it has one.
pub fn ask(hub: Hub, paths: Vec<PathBuf>, parent: Option<String>, d: &crate::Dirs) {
    let (mime_dirs, app_dirs, lists, desktops, programs, config) = (d.mime.clone(), d.apps.clone(), d.mimeapps.clone(), d.desktops.clone(), d.programs.clone(), d.config.clone());
    let lang = crate::language();
    let waiter = hub.clone();
    let spawned = std::thread::Builder::new().name("gannet-open-with".into()).spawn(move || {
        let hub = waiter;
        let Some(exe) = std::env::current_exe().ok() else { return };
        let db = MimeDb::load(&mime_dirs);
        let mut types: Vec<String> = Vec::new();
        for p in &paths {
            let t = file_type(&db, p);
            if !types.contains(&t) {
                types.push(t);
            }
        }
        let what = match paths.as_slice() {
            [one] => format!("“{}”", one.file_name().unwrap_or_default().to_string_lossy()),
            many => crate::format::count(many.len(), "file", "files"),
        };
        let spec = Spec { what: Some(what), offer_always: types.len() == 1, types: types.clone(), parent: parent.map(|h| format!("wayland:{h}")), modal: true, ..Spec::default() };
        let mut command = Command::new(exe);
        command.args(spec.args()).stdin(Stdio::null()).stdout(Stdio::piped());
        // The window goes if the list does.
        crate::portal::die_with_this_thread(&mut command);
        let mut child = match command.spawn() {
            Ok(c) => c,
            Err(e) => return hub.send(Msg::Notice(format!("Couldn't show Open With: {}", gannet_core::listing::describe(&e)))),
        };
        let mut said = String::new();
        if let Some(out) = child.stdout.take() {
            let _ = out.take(MAX_OUTPUT).read_to_string(&mut said);
        }
        let _ = child.wait();
        let Some((id, always)) = read_outcome(&said) else { return };
        let apps = Apps::load(&app_dirs, &lists, &desktops, lang.as_deref(), &programs);
        let Some(app) = apps.get(&id) else { return hub.send(Msg::Notice("That app isn't installed any more".to_string())) };
        if let Some(why) = open_and_remember(app, &paths, always.then_some(types.as_slice()), &config, &lists) {
            hub.send(Msg::Notice(why));
        }
    });
    if spawned.is_err() {
        hub.send(Msg::Notice("Couldn't show Open With".to_string()));
    }
}

/// Opens the files with the app, then, with `default_for` one type, makes it that type's
/// default. An app that couldn't even start isn't made the default. A reason for the user if
/// either went wrong.
fn open_and_remember(app: &apps::App, paths: &[PathBuf], default_for: Option<&[String]>, config: &Path, lists: &[PathBuf]) -> Option<String> {
    if let Some(why) = open_with(app, paths) {
        return Some(why);
    }
    let [mime] = default_for? else { return None };
    let e = apps::set_default(&apps::user_list(config, lists, mime), mime, &app.id).err()?;
    Some(format!("Couldn't make {} the default: {}", app.name, gannet_core::listing::describe(&e).to_lowercase()))
}

/// Starts the app on the files. A reason for the user if it couldn't start.
fn open_with(app: &apps::App, paths: &[PathBuf]) -> Option<String> {
    for line in app.commands(paths, &[TERMINAL.to_string()]) {
        let Some((program, args)) = line.split_first() else { continue };
        let mut cmd = Command::new(program);
        cmd.args(args);
        if let Some(dir) = app.dir.as_ref().filter(|d| d.is_dir()) {
            cmd.current_dir(dir);
        }
        if let Err(e) = gannet_core::open::launch(&mut cmd) {
            let missing = e.kind() == std::io::ErrorKind::NotFound;
            return Some(if missing && app.terminal && program == TERMINAL {
                format!("Couldn't open {}: it runs in a terminal, and {TERMINAL} isn't installed", app.name)
            } else {
                format!("Couldn't open {}: {}", app.name, gannet_core::listing::describe(&e).to_lowercase())
            });
        }
    }
    None
}

/// A file's type, by its name and first bytes, as the rest of Gannet tells it.
fn file_type(db: &MimeDb, path: &Path) -> String {
    let mut head = Vec::new();
    if let Ok(f) = std::fs::File::open(path) {
        let _ = f.take(gannet_core::mime::HEAD as u64).read_to_end(&mut head);
    }
    let name = path.file_name().map_or(&[][..], |n| n.as_encoded_bytes());
    db.guess(name, Some(&head)).to_string()
}

#[cfg(feature = "bench")]
mod bench {
    //! The window's state, for the end-to-end checks to wait on.

    use super::{Row, Window};

    pub struct Bench {
        path: Option<String>,
        last: String,
    }

    impl Bench {
        pub fn new() -> Self {
            Self { path: std::env::var("GANNET_STATE").ok().map(|p| format!("{p}.openwith")), last: String::new() }
        }

        pub fn idle(&mut self, w: &Window) {
            let Some(path) = self.path.clone() else { return };
            let esc = |s: &str| format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""));
            let rows: Vec<String> = w.picker.rows.iter().map(|r| match r {
                Row::Heading(h) => esc(&format!("# {h}")),
                Row::App(c) => esc(&format!("{}|{}{}", c.id, c.name, if c.default { "|default" } else { "" })),
            }).collect();
            let state = format!(
                "{{\"title\":{},\"search\":{},\"rows\":[{}],\"picked\":{},\"always\":{},\"can_always\":{}}}\n",
                esc(&w.picker.spec.title()),
                esc(&w.picker.search.text),
                rows.join(","),
                w.picker.picked().map_or_else(|| "null".to_string(), |c| esc(&c.id)),
                w.picker.always,
                w.picker.can_always(),
            );
            if state != self.last {
                let tmp = format!("{path}.tmp");
                if std::fs::write(&tmp, &state).is_ok() {
                    let _ = std::fs::rename(&tmp, &path);
                }
                self.last = state;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(v: &[&str]) -> Vec<OsString> {
        v.iter().map(OsString::from).collect()
    }

    fn app(name: &str, mimes: &[&str], shown: bool) -> String {
        format!("[Desktop Entry]\nType=Application\nName={name}\nExec=/bin/sh %F\nMimeType={};\n{}", mimes.join(";"), if shown { "" } else { "NoDisplay=true\n" })
    }

    /// Apps in a folder of their own, and a list naming the default for Markdown.
    fn installed() -> (std::path::PathBuf, Apps) {
        let dir = std::env::temp_dir().join(format!("gannet-test-openwith-{}-{}", std::process::id(), rand_tag()));
        let apps_dir = dir.join("applications");
        std::fs::create_dir_all(&apps_dir).unwrap();
        for (id, name, mimes, shown) in [("editor", "Editor", &["text/plain"][..], true), ("notes", "Notes", &["text/markdown"], true), ("viewer", "Image Viewer", &["image/png"], true), ("hidden-md", "Hidden MD", &["text/markdown"], false), ("browser", "Browser", &["text/html"], true)] {
            std::fs::write(apps_dir.join(format!("{id}.desktop")), app(name, mimes, shown)).unwrap();
        }
        let list = dir.join("mimeapps.list");
        std::fs::write(&list, "[Default Applications]\ntext/markdown=notes.desktop\n").unwrap();
        let apps = Apps::load(&[apps_dir], &[list], &[], None, &[]);
        (dir, apps)
    }

    fn rand_tag() -> u64 {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    fn mime_db(dir: &Path) -> MimeDb {
        let m = dir.join("mime");
        std::fs::create_dir_all(m.join("text")).unwrap();
        std::fs::write(m.join("subclasses"), "text/markdown text/plain\n").unwrap();
        std::fs::write(m.join("text/markdown.xml"), "<?xml version=\"1.0\"?>\n<mime-type type=\"text/markdown\"><comment>Markdown document</comment></mime-type>\n").unwrap();
        MimeDb::load(&[m])
    }

    fn rows(p: &Picker) -> Vec<String> {
        p.rows.iter().map(|r| match r {
            Row::Heading(h) => format!("# {h}"),
            Row::App(c) => format!("{}{}", c.id, if c.default { " (default)" } else { "" }),
        }).collect()
    }

    fn key(p: &mut Picker, sym: u32, utf8: &str) -> Option<Finish> {
        p.key(sym, utf8, Mods::default(), 5)
    }

    #[test]
    fn a_spec_survives_the_trip_through_arguments() {
        let s = Spec { what: Some("“a b.md”".into()), types: vec!["text/markdown".into(), "--type".into()], choices: Some(vec!["org.x.Y".into(), "--modal".into()]), last: Some("org.x.Y".into()), offer_always: true, parent: Some("wayland:abc".into()), modal: true };
        let args = s.args();
        assert_eq!(args[0], ARG);
        assert_eq!(Spec::parse(args.into_iter().skip(1)), Some(s));
        let plain = Spec::default();
        assert_eq!(Spec::parse(plain.args().into_iter().skip(1)), Some(plain), "no choices isn't an empty list of them");
        assert_eq!(Spec::parse(os(&["--choice", "x"])), None, "a choice needs --choices first");
        assert_eq!(Spec::parse(os(&["--what"])), None);
        assert_eq!(Spec::parse(os(&["--sideways"])), None);
        assert_eq!(Spec { what: Some("3 files".into()), ..Spec::default() }.title(), "Open 3 files with");
    }

    #[test]
    fn the_outcome_and_new_choices_are_read_back_as_written() {
        assert_eq!(read_outcome(&outcome_text(Some(("org.gnome.Loupe.desktop", true)))), Some(("org.gnome.Loupe.desktop".to_string(), true)));
        assert_eq!(read_outcome(&outcome_text(Some(("x", false)))), Some(("x".to_string(), false)));
        assert_eq!(read_outcome(&outcome_text(None)), None);
        assert_eq!(read_outcome(""), None, "a window that died says nothing");
        assert_eq!(read_outcome("open \n"), None);
        let ids = vec!["a.b".to_string(), "c".to_string()];
        assert_eq!(read_choices_line(&choices_line(&ids)), Some(ids));
        assert_eq!(read_choices_line(&choices_line(&[])), Some(vec![]));
        assert_eq!(read_choices_line("hello"), None);
    }

    #[test]
    fn the_types_apps_come_first_with_the_default_and_then_the_rest() {
        let (dir, apps) = installed();
        let db = mime_db(&dir);
        let spec = Spec { types: vec!["text/markdown".into()], offer_always: true, ..Spec::default() };
        let p = Picker::new(spec, &apps, Some(&db), None);
        // Notes is the default, Hidden MD opens Markdown but isn't listed by itself, and
        // Editor opens it as plain text.
        assert_eq!(rows(&p), ["# For Markdown document", "notes.desktop (default)", "hidden-md.desktop", "editor.desktop", "# Other apps", "browser.desktop", "viewer.desktop"]);
        assert_eq!(p.picked().unwrap().id, "notes.desktop", "starts on the default");
        assert!(p.can_always());
        // Several types: only what opens them all, and no default to make.
        let p = Picker::new(Spec { types: vec!["text/markdown".into(), "text/plain".into()], ..Spec::default() }, &apps, Some(&db), None);
        assert_eq!(rows(&p)[..2], ["# For all of these", "editor.desktop"]);
        assert!(!p.can_always());
        // Nothing for the type: the others, with the cursor on the first.
        let p = Picker::new(Spec { types: vec!["video/mp4".into()], ..Spec::default() }, &apps, Some(&db), None);
        assert_eq!(rows(&p), ["# Other apps", "browser.desktop", "editor.desktop", "viewer.desktop", "notes.desktop"]);
        assert_eq!(p.picked().unwrap().id, "browser.desktop");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn typing_searches_and_keys_pick() {
        let (dir, apps) = installed();
        let db = mime_db(&dir);
        let mut p = Picker::new(Spec { types: vec!["text/markdown".into()], offer_always: true, ..Spec::default() }, &apps, Some(&db), None);
        assert_eq!(key(&mut p, k::KEY_space, " "), None);
        assert!(p.always, "Space ticks it with no search");
        key(&mut p, k::KEY_Down, "");
        assert_eq!(p.picked().unwrap().id, "hidden-md.desktop");
        key(&mut p, k::KEY_End, "");
        assert_eq!(p.picked().unwrap().id, "viewer.desktop", "headings are stepped over");
        key(&mut p, k::KEY_Up, "");
        assert_eq!(p.picked().unwrap().id, "browser.desktop");
        for ch in ["v", "I", "e"] {
            key(&mut p, 0, ch);
        }
        assert_eq!(rows(&p), ["# Other apps", "viewer.desktop"], "by name, any case");
        assert_eq!(p.picked().unwrap().id, "viewer.desktop", "the cursor goes to what's left");
        key(&mut p, k::KEY_space, " ");
        assert_eq!(p.search.text, "vIe ", "a space once there's a search");
        key(&mut p, 0, "x");
        assert!(p.picked().is_none() && p.rows.is_empty());
        assert_eq!(key(&mut p, k::KEY_Return, ""), None, "nothing to open");
        assert_eq!(key(&mut p, k::KEY_Escape, ""), None, "Escape clears the search first");
        assert_eq!(p.search.text, "");
        assert_eq!(p.rows.len(), 7);
        key(&mut p, 0, "brow");
        assert_eq!(key(&mut p, k::KEY_Return, ""), Some(Finish::Open("browser.desktop".into())));
        key(&mut p, k::KEY_BackSpace, "");
        assert_eq!(p.search.text, "bro");
        key(&mut p, k::KEY_Escape, "");
        assert_eq!(key(&mut p, k::KEY_Escape, ""), Some(Finish::Cancel));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_app_that_takes_no_files_isnt_offered() {
        let (dir, _) = installed();
        std::fs::write(dir.join("applications/settings.desktop"), "[Desktop Entry]\nType=Application\nName=Settings\nExec=/bin/sh\nMimeType=video/mp4;\n").unwrap();
        let apps = Apps::load(&[dir.join("applications")], &[], &[], None, &[]);
        let p = Picker::new(Spec { types: vec!["video/mp4".into()], ..Spec::default() }, &apps, None, None);
        assert!(!rows(&p).iter().any(|r| r.starts_with("settings")), "not even for a type it names: {:?}", rows(&p));
        assert_eq!(rows(&p).len(), 5);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_app_that_cant_start_isnt_made_the_default() {
        let dir = std::env::temp_dir().join(format!("gannet-test-openwith-default-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let config = dir.join("config");
        let lists = vec![config.join("mimeapps.list")];
        let app = |exec: &str| apps::App { id: "e.desktop".into(), name: "E".into(), exec: vec![exec.into(), "%F".into()], icon: None, terminal: false, dir: None, file: "/e.desktop".into(), mimes: vec![], shown: true };
        let types = ["text/plain".to_string()];
        let files = [dir.join("a.txt")];
        let why = open_and_remember(&app("/nonexistent/editor"), &files, Some(&types), &config, &lists);
        assert!(why.is_some_and(|w| w.starts_with("Couldn't open E")));
        assert!(!lists[0].exists(), "not made the default");
        assert_eq!(open_and_remember(&app("/bin/true"), &files, None, &config, &lists), None);
        assert!(!lists[0].exists(), "not asked to");
        assert_eq!(open_and_remember(&app("/bin/true"), &files, Some(&types), &config, &lists), None);
        assert_eq!(std::fs::read_to_string(&lists[0]).unwrap(), "[Default Applications]\ntext/plain=e.desktop;\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_programs_own_list_is_all_that_shows() {
        let (dir, apps) = installed();
        let spec = Spec { choices: Some(vec!["viewer".into(), "not-installed".into(), "editor".into(), "viewer.desktop".into()]), last: Some("editor".into()), ..Spec::default() };
        let mut p = Picker::new(spec, &apps, None, None);
        // Answered with the names the program gave, without headings or other apps.
        assert_eq!(rows(&p), ["viewer", "editor"]);
        assert_eq!(p.picked().unwrap().id, "editor", "on the last one picked");
        assert!(!p.can_always(), "the program decides what to remember");
        key(&mut p, k::KEY_space, " ");
        assert!(!p.always);
        p.set_choices(vec!["browser".into(), "editor".into()], &apps);
        assert_eq!(rows(&p), ["browser", "editor"]);
        assert_eq!(p.picked().unwrap().id, "editor", "still on the same one");
        p.set_choices(vec![], &apps);
        assert!(p.picked().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_screen_reader_sees_the_apps_and_picks_one() {
        use crate::a11y::{self, Act, Focus, Role, TextEdit};
        let (dir, apps) = installed();
        let db = mime_db(&dir);
        let spec = Spec { types: vec!["text/markdown".into()], offer_always: true, ..Spec::default() };
        let picker = Picker::new(spec, &apps, Some(&db), None);
        let mut w = Window { picker, apps, text: Text::new(crate::text::UI_PX, false), colors: theme::builtin(true), scale: 1.0, w: 520, h: 560, scroll: 0, pointer: (0, 0), hover: Hit::None, last_click: None, dirty: false, finish: None, has_keyboard: true, a11y_ids: HashMap::new() };
        let snap = w.a11y_snapshot(a11y::Rows::default(), None);
        let list = snap.get(a11y::OPEN_WITH_APPS).unwrap();
        let items: Vec<(Role, &str, &str)> = list.children.iter().map(|&id| snap.get(id).unwrap()).map(|n| (n.role, n.name.as_str(), n.description.as_str())).collect();
        assert_eq!(items[..3], [(Role::Heading, "For Markdown document", ""), (Role::ListItem, "Notes", "Default"), (Role::ListItem, "Hidden MD", "")]);
        let notes = list.children[1];
        assert_eq!(snap.focus, Focus::Node(notes), "the app picked has the keyboard");
        assert_eq!(snap.get(a11y::FRAME).unwrap().role, Role::Dialog);
        assert!(snap.get(a11y::OPEN_WITH_ALWAYS).is_some_and(|n| n.checked == Some(false)));
        // An app keeps its number as a search narrows the list.
        let editor = list.children[3];
        w.picker.search = crate::field::Field::new("edit");
        w.picker.rebuild();
        let snap = w.a11y_snapshot(a11y::Rows::default(), None);
        assert_eq!(snap.get(a11y::OPEN_WITH_APPS).unwrap().children[1], editor);
        assert_eq!(snap.focus, Focus::Node(editor));
        w.picker.search = crate::field::Field::default();
        w.picker.rebuild();
        // Typing in the search through a screen reader narrows the apps, as on the keyboard,
        // and the search's characters are placed for it.
        w.a11y_act(Act::EditText(a11y::OPEN_WITH_SEARCH, 0, TextEdit::Replace(0, 0, "edit".into())));
        assert_eq!((w.picker.search.text.as_str(), w.picker.picked().unwrap().id.as_str()), ("edit", "editor.desktop"));
        let snap = w.a11y_snapshot(a11y::Rows::default(), None);
        assert_eq!(snap.geometry(a11y::OPEN_WITH_SEARCH).map(|g| g.edges.len()), Some(5));
        w.a11y_act(Act::EditText(a11y::OPEN_WITH_SEARCH, 0, TextEdit::Select(0, 2)));
        assert_eq!(w.picker.search.selection(), (0, 2));
        w.a11y_act(Act::EditText(a11y::OPEN_WITH_SEARCH, 0, TextEdit::Paste(0)));
        assert_eq!(w.picker.search.text, "edit", "there's no clipboard here");
        w.a11y_act(Act::EditText(a11y::OPEN_WITH_SEARCH, 0, TextEdit::Replace(0, usize::MAX, String::new())));
        assert!(w.picker.search.text.is_empty() && w.picker.rows.len() > 3, "every app again");
        // Moving to an app picks it; ticking and its action open with it.
        w.a11y_act(Act::Focus(notes));
        assert_eq!(w.picker.picked().unwrap().id, "notes.desktop");
        w.a11y_act(Act::Focus(a11y::OPEN_WITH_APPS));
        assert_eq!(w.picker.picked().unwrap().id, "notes.desktop", "the list itself isn't an app");
        w.a11y_act(Act::Press(a11y::OPEN_WITH_ALWAYS));
        assert!(w.picker.always);
        w.a11y_act(Act::Press(editor));
        assert_eq!(w.finish, Some(Finish::Open("editor.desktop".into())));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
