//! The accessibility snapshot (`a11y`) served over AT-SPI, written by hand on zbus.
//!
//! The session watcher starts a server when a screen reader turns on and stops it when it
//! goes, so nothing here runs, and no connection is open, while no one is listening. A server
//! connects to the accessibility bus and embeds the app under the registry's desktop. Calls
//! from screen readers are answered on the thread following the bus, from the latest snapshot
//! the UI thread handed over, so the UI never waits on them. Its other thread turns each new
//! snapshot into the events that say what changed.
//!
//! A row's object path is the folder visit (`a11y::Rows::era`) and its key (`Listing::key`:
//! its file's name, or its path in the Trash and Recent views), in hex.
//! The same file keeps its path however often the folder is read again, and a path can never
//! stand for another file, so a screen reader can't be handed a name it kept from before. The
//! list says it manages its descendants, so screen readers don't keep its children either,
//! and a big change needn't be announced row by row.
//!
//! The list is a table: each row has a cell for each column, whose path is the row's with the
//! column's place in front. Notices, jobs that finish and a new sort order are announced, as
//! Orca presents an `Announcement` straight away; a job's progress is a progress bar's value.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex};

use gannet_core::bus::{Bus, zbus};
use zbus::message::{Body, Header, Type};
use zbus::{MatchRule, Message, OwnedObjectPath, Variant};

use crate::a11y::{self, Act, Node, Role, Rows, Shared, Snapshot, TextEdit, ToServer};
use crate::app::{Hub, Msg};
use crate::field::Field;

const ROOT: &str = "/org/a11y/atspi/accessible/root";
const PREFIX: &str = "/org/a11y/atspi/accessible/";
const NULL: &str = "/org/a11y/atspi/null";
const CACHE: &str = "/org/a11y/atspi/cache";
const REGISTRY: &str = "org.a11y.atspi.Registry";
const ACCESSIBLE: &str = "org.a11y.atspi.Accessible";
const APPLICATION: &str = "org.a11y.atspi.Application";
const COMPONENT: &str = "org.a11y.atspi.Component";
const ACTION: &str = "org.a11y.atspi.Action";
const SELECTION: &str = "org.a11y.atspi.Selection";
const TABLE: &str = "org.a11y.atspi.Table";
const TABLE_CELL: &str = "org.a11y.atspi.TableCell";
const VALUE: &str = "org.a11y.atspi.Value";
const TEXT: &str = "org.a11y.atspi.Text";
const EDITABLE_TEXT: &str = "org.a11y.atspi.EditableText";
const PROPS: &str = "org.freedesktop.DBus.Properties";
const OBJECT_EVENT: &str = "org.a11y.atspi.Event.Object";
const WINDOW_EVENT: &str = "org.a11y.atspi.Event.Window";
const UNKNOWN_OBJECT: &str = "org.freedesktop.DBus.Error.UnknownObject";
const UNKNOWN_METHOD: &str = "org.freedesktop.DBus.Error.UnknownMethod";

// AtspiRole and AtspiStateType numbers.
const ROLE_APPLICATION: u32 = 75;
const STATE_ACTIVE: u32 = 1;
const STATE_CHECKED: u32 = 4;
const STATE_EDITABLE: u32 = 7;
const STATE_SELECTABLE_TEXT: u32 = 38;
const STATE_ENABLED: u32 = 8;
const STATE_FOCUSABLE: u32 = 11;
const STATE_FOCUSED: u32 = 12;
const STATE_MODAL: u32 = 16;
const STATE_INDETERMINATE: u32 = 32;
const STATE_MULTISELECTABLE: u32 = 18;
const STATE_SELECTABLE: u32 = 22;
const STATE_SELECTED: u32 = 23;
const STATE_SENSITIVE: u32 = 24;
const STATE_SHOWING: u32 = 25;
const STATE_SINGLE_LINE: u32 = 26;
const STATE_VISIBLE: u32 = 30;
const STATE_MANAGES_DESCENDANTS: u32 = 31;
const STATE_IS_DEFAULT: u32 = 39;
const STATE_CHECKABLE: u32 = 41;
const STATE_READ_ONLY: u32 = 43;

// AtspiLive: how urgently an announcement is to be made.
const POLITE: i32 = 1;
const ASSERTIVE: i32 = 2;

/// More rows than this changing at once aren't announced one by one. The list manages its
/// descendants, so screen readers don't rely on hearing each.
const ROW_EVENTS: usize = 64;
/// How many calls zbus holds while one is answered. A screen reader walking the window
/// sends them in bursts, and past this the connection stalls until they're taken.
const CALL_QUEUE: usize = 4096;

/// An object as AT-SPI names it: the connection, and the path there.
type Ref = (String, OwnedObjectPath);
/// An object as the Cache interface describes it, though nothing is ever cached here.
type CacheItem = (Ref, Ref, Ref, i32, i32, Vec<String>, String, u32, String, Vec<u32>);

/// Serves the app on the accessibility bus at `address` until `shared` is disconnected, the
/// connection closes or it can't start. Returns the server's number at once, or None if its
/// thread couldn't start; the work is on threads of its own. `stopped` is called with that
/// number once it has stopped, however it stopped.
pub fn start(shared: Arc<Shared>, hub: Hub, address: String, stopped: impl FnOnce(u64) + Send + 'static) -> Option<u64> {
    let epoch = shared.next_epoch();
    let spawned = std::thread::Builder::new().name("gannet-a11y".into()).spawn(move || {
        if let Err(e) = run(&shared, epoch, hub, &address) {
            eprintln!("gannet: accessibility: {e}");
        }
        shared.stopped(epoch);
        stopped(epoch);
    });
    match spawned {
        Ok(_) => Some(epoch),
        Err(e) => {
            eprintln!("gannet: accessibility: couldn't start a thread: {e}");
            None
        }
    }
}

/// Tells the server its connection closed: the thread following the bus drops it when the
/// connection ends, whether or not anything was said first.
struct Lost(std::sync::mpsc::Sender<ToServer>);

impl Drop for Lost {
    fn drop(&mut self) {
        let _ = self.0.send(ToServer::Lost);
    }
}

fn run(shared: &Shared, epoch: u64, hub: Hub, address: &str) -> zbus::Result<()> {
    let bus = Bus::connect(address)?;
    let me = bus.connection().unique_name().map(|n| n.to_string()).unwrap_or_default();
    let ctx = Arc::new(Ctx { bus: bus.clone(), me, now: Mutex::default(), hub: hub.clone(), desktop: Mutex::new(null()), id: Mutex::new(0), served: Mutex::default(), incarnations: Mutex::default() });
    let (tx, rx) = channel();
    let calls = MatchRule::builder().msg_type(Type::MethodCall).build()?;
    // The registry starting again forgets every app, so it's introduced again.
    let registry = MatchRule::builder().msg_type(Type::Signal).sender("org.freedesktop.DBus").member("NameOwnerChanged").arg(0, REGISTRY).build()?;
    let (ctx2, lost) = (ctx.clone(), Lost(tx.clone()));
    let _following = bus.follow_all(vec![calls, registry], Some(CALL_QUEUE), move |m| {
        // zbus hands over an error only as the connection closes.
        let Ok(m) = m else { return false };
        if m.message_type() == Type::MethodCall {
            ctx2.dispatch(&m);
        } else if m.body().deserialize::<(String, String, String)>().is_ok_and(|(_, _, new)| !new.is_empty()) {
            let _ = lost.0.send(ToServer::Embed);
        }
        true
    })?;
    if !shared.connect(epoch, tx) {
        return Ok(());
    }
    // The app is introduced only once there's a snapshot to answer from: the registry and
    // screen readers ask about it straight away.
    hub.send(Msg::A11y(Act::Wake));
    let mut old = loop {
        match rx.recv() {
            Ok(ToServer::Snapshot(s)) => break Arc::new(*s),
            // Not introduced yet, so there's nothing to introduce again.
            Ok(ToServer::Embed) => {}
            Ok(ToServer::Stop) | Err(_) => return Ok(()),
            Ok(ToServer::Lost) => return Err(zbus::Error::Failure("lost the accessibility bus".into())),
        }
    };
    *ctx.now.lock().unwrap() = Some(old.clone());
    ctx.embed()?;
    ctx.remember(&old);
    // A window that opens with the keyboard says so, as one that gains it later does. A
    // screen reader that's already running only turns to a new window when it's told, so
    // without this the preview, the Open and Save dialogs and Open With open in silence.
    if old.window_focused {
        let before = Snapshot { window_focused: false, ..(*old).clone() };
        ctx.emit_changes(&before, &old);
    }
    for m in rx {
        match m {
            ToServer::Snapshot(s) => {
                let new = Arc::new(*s);
                *ctx.now.lock().unwrap() = Some(new.clone());
                ctx.emit_changes(&old, &new);
                ctx.remember(&new);
                old = new;
            }
            ToServer::Embed => {
                if let Err(e) = ctx.embed() {
                    eprintln!("gannet: accessibility: the registry restarted, and joining it again failed: {e}");
                }
            }
            ToServer::Stop => break,
            ToServer::Lost => return Err(zbus::Error::Failure("lost the accessibility bus".into())),
        }
    }
    Ok(())
}

fn null() -> Ref {
    (String::new(), OwnedObjectPath::try_from(NULL).unwrap())
}

/// What an object path names.
enum Target<'a> {
    Root,
    Fixed(&'a Node),
    /// A row: its listing entry and its place in the list.
    Row(u32, usize),
    /// A row's cell: its listing entry, the row's place in the list, and its column's place
    /// among the columns.
    Cell(u32, usize, usize),
}

fn fixed_path(id: u64) -> String {
    format!("{PREFIX}{id}")
}

fn row_path(era: u64, key: &[u8]) -> String {
    use std::fmt::Write;
    let mut p = format!("{PREFIX}r{era}_");
    for b in key {
        let _ = write!(p, "{b:02x}");
    }
    p
}

/// A row's cell's path: the row's, with the column's place among the columns.
fn cell_path(era: u64, col: usize, key: &[u8]) -> String {
    row_path(era, key).replacen(&format!("{PREFIX}r"), &format!("{PREFIX}c{col}_"), 1)
}

/// The folder visit and key a row's path names.
fn parse_row(rest: &str) -> Option<(u64, Vec<u8>)> {
    let (era, hex) = rest.split_once('_')?;
    if hex.len() % 2 != 0 {
        return None;
    }
    let name = (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok()).collect::<Option<Vec<u8>>>()?;
    Some((era.parse().ok()?, name))
}

fn role_of(r: Role) -> (u32, &'static str) {
    match r {
        Role::Frame => (23, "frame"),
        Role::Dialog => (16, "dialog"),
        Role::List => (31, "list"),
        Role::ListItem => (32, "list item"),
        Role::PushButton => (43, "push button"),
        Role::CheckBox => (7, "check box"),
        Role::Entry => (79, "entry"),
        Role::Label => (29, "label"),
        Role::Heading => (83, "heading"),
        Role::Document => (82, "document frame"),
        Role::Table => (55, "table"),
        Role::ColumnHeader => (10, "column header"),
        Role::Link => (88, "link"),
        Role::ProgressBar => (42, "progress bar"),
        Role::Panel => (39, "panel"),
        Role::Alert => (2, "alert"),
        Role::PasswordText => (40, "password text"),
        Role::Menu => (33, "menu"),
        Role::MenuItem => (35, "menu item"),
    }
}

/// How much of a field's text a screen reader asks for at a time.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Unit {
    Char,
    /// A word and what follows it up to the next one.
    Word,
    /// All of it.
    Line,
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric()
}

/// The piece of text around a character, in characters: where it starts and where it ends.
/// Past the last character there's nothing.
fn segment(chars: &[char], at: usize, unit: Unit) -> (usize, usize) {
    let len = chars.len();
    if at >= len {
        return if unit == Unit::Line { (0, len) } else { (len, len) };
    }
    match unit {
        Unit::Char => (at, at + 1),
        Unit::Line => (0, len),
        Unit::Word => {
            let starts = |i: usize| i < len && is_word(chars[i]) && (i == 0 || !is_word(chars[i - 1]));
            let a = (0..=at).rev().find(|&i| starts(i)).unwrap_or(0);
            let b = (at + 1..len).find(|&i| starts(i)).unwrap_or(len);
            (a, b)
        }
    }
}

/// The piece before the one around a character.
fn before(chars: &[char], at: usize, unit: Unit) -> (usize, usize) {
    // Past the end, the last piece is the one before.
    let start = if at >= chars.len() { chars.len() } else { segment(chars, at, unit).0 };
    if unit == Unit::Line || start == 0 {
        return (0, 0);
    }
    segment(chars, start - 1, unit)
}

/// The piece after the one around a character.
fn after(chars: &[char], at: usize, unit: Unit) -> (usize, usize) {
    let (_, b) = segment(chars, at, unit);
    if unit == Unit::Line || b >= chars.len() {
        return (chars.len(), chars.len());
    }
    segment(chars, b, unit)
}

/// What a change to a field's text took out and put in, and where: the text that differs
/// between what the two have in common at their starts and their ends.
fn text_change(old: &str, new: &str) -> (usize, String, String) {
    let (o, n): (Vec<char>, Vec<char>) = (old.chars().collect(), new.chars().collect());
    let head = o.iter().zip(&n).take_while(|(a, b)| a == b).count();
    let room = o.len().min(n.len()) - head;
    let tail = o.iter().rev().zip(n.iter().rev()).take(room).take_while(|(a, b)| a == b).count();
    (head, o[head..o.len() - tail].iter().collect(), n[head..n.len() - tail].iter().collect())
}

fn bits(on: &[u32]) -> Vec<u32> {
    let mut b = vec![0u32; 2];
    for s in on {
        b[(s / 32) as usize] |= 1 << (s % 32);
    }
    b
}

struct Ctx {
    bus: Bus,
    me: String,
    /// The latest snapshot, once there is one.
    now: Mutex<Option<Arc<Snapshot>>>,
    hub: Hub,
    desktop: Mutex<Ref>,
    /// The id the registry gave the app.
    id: Mutex<i32>,
    /// What each part was last served as: its role, name and description. A menu's or
    /// dialog's parts come back under the same ids as another's.
    served: Mutex<HashMap<u64, (Role, String, String)>>,
    /// How many times a part has come back as something else, which is in its path after its
    /// id, so it's a new object each time.
    incarnations: Mutex<HashMap<u64, u32>>,
}

impl Ctx {
    fn remember(&self, s: &Snapshot) {
        let mut served = self.served.lock().unwrap();
        for n in &s.fixed {
            match served.get_mut(&n.id) {
                Some(was) if was.0 == n.role && was.1 == n.name && was.2 == n.description => {}
                Some(was) => *was = (n.role, n.name.clone(), n.description.clone()),
                None => {
                    served.insert(n.id, (n.role, n.name.clone(), n.description.clone()));
                }
            }
        }
    }

    fn embed(&self) -> zbus::Result<()> {
        let (desktop,): (Ref,) = self.bus.call(REGISTRY, ROOT, "org.a11y.atspi.Socket", "Embed", &((self.me.clone(), OwnedObjectPath::try_from(ROOT)?),))?;
        *self.desktop.lock().unwrap() = desktop;
        Ok(())
    }

    /// A part's path: its id, and how many times it's come back as something else, if it has.
    fn path(&self, id: u64) -> String {
        match self.incarnations.lock().unwrap().get(&id) {
            Some(&n) if n > 0 => format!("{}_{n}", fixed_path(id)),
            _ => fixed_path(id),
        }
    }

    /// The part a path names, if it's still that part.
    fn part<'a>(&self, rest: &str, snap: &'a Snapshot) -> Option<&'a Node> {
        let (id, n) = rest.split_once('_').map_or(Some((rest, 0)), |(id, n)| Some((id, n.parse::<u32>().ok().filter(|&n| n > 0)?)))?;
        let id: u64 = id.parse().ok()?;
        (self.incarnations.lock().unwrap().get(&id).copied().unwrap_or(0) == n).then(|| snap.get(id)).flatten()
    }

    fn at(&self, path: &str) -> Ref {
        (self.me.clone(), OwnedObjectPath::try_from(path).unwrap_or_else(|_| OwnedObjectPath::try_from(NULL).unwrap()))
    }

    fn fixed_ref(&self, id: u64) -> Ref {
        self.at(&self.path(id))
    }

    fn row_ref(&self, rows: &Rows, entry: u32) -> Ref {
        self.at(&row_path(rows.era, &rows.key(entry)))
    }

    fn cell_ref(&self, rows: &Rows, entry: u32, col: usize) -> Ref {
        self.at(&cell_path(rows.era, col, &rows.key(entry)))
    }

    fn reply<B: zbus::export::serde::Serialize + zbus::DynamicType>(&self, h: &Header<'_>, body: &B) {
        let _ = zbus::block_on(self.bus.connection().reply(h, body));
    }

    fn error(&self, h: &Header<'_>, name: &str, text: &str) {
        let _ = zbus::block_on(self.bus.connection().reply_error(h, name, &(text,)));
    }

    fn signal<B: zbus::export::serde::Serialize + zbus::DynamicType>(&self, path: &str, iface: &str, member: &str, body: &B) {
        let _ = zbus::block_on(self.bus.connection().emit_signal(None::<&str>, path, iface, member, body));
    }

    /// An AT-SPI event: its kind and a number, and a value.
    fn event(&self, path: &str, iface: &str, member: &str, (kind, d1): (&str, i32), data: Variant<'_>) {
        self.event2(path, iface, member, (kind, d1, 0), data);
    }

    /// An event with two numbers, as a text change says where and how long.
    fn event2(&self, path: &str, iface: &str, member: &str, (kind, d1, d2): (&str, i32, i32), data: Variant<'_>) {
        self.signal(path, iface, member, &(kind, d1, d2, data, HashMap::<&str, Variant<'_>>::new()));
    }

    fn dispatch(&self, m: &Message) {
        let h = m.header();
        let (Some(path), Some(member)) = (h.path(), h.member()) else { return };
        let iface = h.interface().map(|i| i.as_str()).unwrap_or("");
        let path = path.as_str();
        if path == CACHE {
            // Nothing is cached: clients ask for what they need.
            let none: Vec<CacheItem> = Vec::new();
            return self.reply(&h, &(none,));
        }
        let Some(snap) = self.now.lock().unwrap().clone() else { return self.error(&h, UNKNOWN_OBJECT, "not ready") };
        let target = if path == ROOT {
            Target::Root
        } else if let Some(rest) = path.strip_prefix(PREFIX) {
            // A row's path, or a cell's, names a row of this folder visit that's still listed.
            let row = |r: &str| parse_row(r).filter(|(era, _)| *era == snap.rows.era).and_then(|(_, key)| snap.rows.position_of(&key)).and_then(|at| Some((snap.rows.entry(at)?, at)));
            let cell = rest.strip_prefix('c').and_then(|c| c.split_once('_')).and_then(|(col, r)| Some((col.parse::<usize>().ok().filter(|&c| c < snap.rows.columns.len())?, r)));
            let found = match (cell, rest.strip_prefix('r')) {
                (Some((col, r)), _) => row(r).map(|(e, at)| Target::Cell(e, at, col)),
                (None, Some(r)) => row(r).map(|(e, at)| Target::Row(e, at)),
                (None, None) => self.part(rest, &snap).map(Target::Fixed),
            };
            match found {
                Some(t) => t,
                None => return self.error(&h, UNKNOWN_OBJECT, "gone"),
            }
        } else {
            return self.error(&h, UNKNOWN_OBJECT, "no such object");
        };
        let body = m.body();
        // Nothing behind a menu or dialog can be used, as with the pointer: asking it to do
        // anything does nothing, and says so.
        let behind = match &target {
            Target::Root => false,
            Target::Fixed(n) => snap.behind(n.id),
            Target::Row(..) | Target::Cell(..) => snap.rows_behind(),
        };
        let request = matches!(
            (iface, member.as_str()),
            (ACTION, "DoAction")
                | (COMPONENT, "GrabFocus" | "ScrollTo" | "ScrollToPoint")
                | (SELECTION, "SelectChild" | "DeselectChild" | "DeselectSelectedChild" | "SelectAll" | "ClearSelection")
                | (TABLE, "AddRowSelection" | "RemoveRowSelection")
                | (TEXT, "SetCaretOffset" | "AddSelection" | "SetSelection" | "RemoveSelection")
                | (EDITABLE_TEXT, "SetTextContents" | "InsertText" | "CutText" | "DeleteText" | "PasteText")
        );
        // A passphrase is typed with the keyboard only: its text here is dots, which mustn't
        // become the passphrase.
        let passphrase = matches!(&target, Target::Fixed(n) if n.role == Role::PasswordText);
        if (behind || passphrase) && request {
            return self.reply(&h, &(false,));
        }
        match (iface, member.as_str()) {
            (PROPS, "Get") => {
                let Ok((i, name)) = body.deserialize::<(String, String)>() else { return self.error(&h, "org.freedesktop.DBus.Error.InvalidArgs", "") };
                match self.props(&target, &snap, &i).into_iter().find(|(n, _)| *n == name) {
                    Some((_, v)) => self.reply(&h, &(v,)),
                    None => self.error(&h, "org.freedesktop.DBus.Error.UnknownProperty", &name),
                }
            }
            (PROPS, "GetAll") => {
                let i: String = body.deserialize().unwrap_or_default();
                let all: HashMap<&str, Variant<'_>> = self.props(&target, &snap, &i).into_iter().collect();
                self.reply(&h, &(all,));
            }
            (PROPS, "Set") => {
                if let Ok((_, name, v)) = body.deserialize::<(String, String, zbus::OwnedVariant)>()
                    && name == "Id"
                {
                    *self.id.lock().unwrap() = i32::try_from(&*v).unwrap_or(0);
                }
                self.reply(&h, &());
            }
            (ACCESSIBLE, m) => self.accessible(&h, m, &body, &target, &snap),
            (COMPONENT, m) => self.component(&h, m, &body, &target, &snap),
            (ACTION, m) => self.action(&h, m, &body, &target, &snap),
            (SELECTION, m) => self.selection(&h, m, &body, &target, &snap),
            (TABLE, m) => self.table(&h, m, &body, &target, &snap),
            (TABLE_CELL, m) => self.table_cell(&h, m, &target, &snap),
            (TEXT, m) => self.text(&h, m, &body, &target, &snap),
            (EDITABLE_TEXT, m) => self.editable_text(&h, m, &body, &target, &snap),
            (APPLICATION, "GetLocale") => self.reply(&h, &("",)),
            (APPLICATION, "GetApplicationBusAddress") => self.reply(&h, &("",)),
            _ => self.error(&h, UNKNOWN_METHOD, member.as_str()),
        }
    }

    fn child_count(&self, target: &Target<'_>, snap: &Snapshot) -> usize {
        match target {
            Target::Root => 1,
            Target::Fixed(n) if n.id == a11y::LIST => snap.rows.len(),
            Target::Fixed(n) => n.children.len(),
            Target::Row(..) => snap.rows.columns.len(),
            Target::Cell(..) => 0,
        }
    }

    fn child(&self, target: &Target<'_>, snap: &Snapshot, i: usize) -> Option<Ref> {
        match target {
            Target::Root => (i == 0).then(|| self.fixed_ref(a11y::FRAME)),
            Target::Fixed(n) if n.id == a11y::LIST => snap.rows.entry(i).map(|e| self.row_ref(&snap.rows, e)),
            Target::Fixed(n) => n.children.get(i).map(|&c| self.fixed_ref(c)),
            Target::Row(e, _) => (i < snap.rows.columns.len()).then(|| self.cell_ref(&snap.rows, *e, i)),
            Target::Cell(..) => None,
        }
    }

    fn parent(&self, target: &Target<'_>, snap: &Snapshot) -> Ref {
        match target {
            Target::Root => self.desktop.lock().unwrap().clone(),
            Target::Fixed(n) if n.parent == 0 => self.at(ROOT),
            Target::Fixed(n) => self.fixed_ref(n.parent),
            Target::Row(..) => self.fixed_ref(a11y::LIST),
            Target::Cell(e, ..) => self.row_ref(&snap.rows, *e),
        }
    }

    fn index_in_parent(&self, target: &Target<'_>, snap: &Snapshot) -> i32 {
        match target {
            Target::Root => -1,
            Target::Fixed(n) if n.parent == 0 => 0,
            Target::Fixed(n) => snap.get(n.parent).and_then(|p| p.children.iter().position(|&c| c == n.id)).map_or(-1, |i| i as i32),
            Target::Row(_, at) => *at as i32,
            Target::Cell(_, _, col) => *col as i32,
        }
    }

    fn name(&self, target: &Target<'_>, snap: &Snapshot) -> String {
        match target {
            Target::Root => "Gannet".into(),
            Target::Fixed(n) => n.name.clone(),
            Target::Row(e, _) => snap.rows.name(*e),
            Target::Cell(e, _, col) => snap.rows.columns.get(*col).map(|c| snap.rows.cell(*e, c.column, gannet_core::time::now())).unwrap_or_default(),
        }
    }

    /// What a row says besides its name, the rest of its cells, or what a part says besides
    /// its name.
    fn description(&self, target: &Target<'_>, snap: &Snapshot) -> String {
        match target {
            Target::Row(e, _) => snap.rows.summary(*e, gannet_core::time::now()),
            Target::Fixed(n) => n.description.clone(),
            Target::Root | Target::Cell(..) => String::new(),
        }
    }

    fn role(&self, target: &Target<'_>) -> (u32, &'static str) {
        match target {
            Target::Root => (ROLE_APPLICATION, "application"),
            Target::Fixed(n) => role_of(n.role),
            Target::Row(..) => (90, "table row"),
            Target::Cell(..) => (56, "table cell"),
        }
    }

    fn states(&self, target: &Target<'_>, snap: &Snapshot) -> Vec<u32> {
        let focused = |f: a11y::Focus| snap.window_focused && snap.focus == f;
        match target {
            Target::Root => bits(&[]),
            Target::Fixed(n) => {
                let mut on = vec![STATE_VISIBLE];
                if n.showing {
                    on.push(STATE_SHOWING);
                }
                if n.enabled && !snap.behind(n.id) {
                    on.extend([STATE_ENABLED, STATE_SENSITIVE]);
                }
                match n.role {
                    Role::Frame | Role::Dialog if snap.window_focused => on.push(STATE_ACTIVE),
                    // The rename dialog's names can only be read.
                    Role::ListItem if n.parent == a11y::RENAME_NAMES => {}
                    Role::ListItem => on.extend([STATE_SELECTABLE, STATE_FOCUSABLE]),
                    Role::Menu => on.push(STATE_FOCUSABLE),
                    Role::PushButton => on.push(STATE_FOCUSABLE),
                    Role::CheckBox => on.extend([STATE_FOCUSABLE, STATE_CHECKABLE]),
                    Role::Entry => on.extend([STATE_FOCUSABLE, STATE_EDITABLE, STATE_SINGLE_LINE, STATE_SELECTABLE_TEXT]),
                    Role::PasswordText => on.extend([STATE_FOCUSABLE, STATE_EDITABLE, STATE_SINGLE_LINE]),
                    Role::MenuItem => on.extend([STATE_SELECTABLE, STATE_FOCUSABLE]),
                    Role::Document => on.extend([STATE_FOCUSABLE, STATE_READ_ONLY]),
                    Role::Table => on.extend([STATE_FOCUSABLE, STATE_MULTISELECTABLE, STATE_MANAGES_DESCENDANTS]),
                    Role::ProgressBar if snap.news.jobs.iter().any(|j| a11y::JOB0 + j.id == n.id && j.percent.is_none()) => on.push(STATE_INDETERMINATE),
                    Role::List => on.push(STATE_FOCUSABLE),
                    _ => {}
                }
                // A dialog keeps the keyboard until it's answered.
                if matches!(n.role, Role::Dialog | Role::Alert) && n.parent != 0 {
                    on.push(STATE_MODAL);
                }
                // Only while it can be pressed: Enter does nothing otherwise.
                if n.enabled && snap.modal.is_some_and(|m| m.default == Some(n.id)) {
                    on.push(STATE_IS_DEFAULT);
                }
                // The folder shown is marked as the current part of the path, not as selected.
                if n.selected && n.role != Role::Link {
                    on.push(STATE_SELECTED);
                }
                if n.checked == Some(true) {
                    on.push(STATE_CHECKED);
                }
                if focused(a11y::Focus::Node(n.id)) {
                    on.push(STATE_FOCUSED);
                }
                bits(&on)
            }
            Target::Row(e, at) => {
                let mut on = vec![STATE_VISIBLE, STATE_SELECTABLE, STATE_FOCUSABLE];
                if !snap.rows_behind() {
                    on.extend([STATE_ENABLED, STATE_SENSITIVE]);
                }
                if snap.rows.showing(*at) {
                    on.push(STATE_SHOWING);
                }
                if snap.rows.is_selected(*e) {
                    on.push(STATE_SELECTED);
                }
                if focused(a11y::Focus::Row(*e)) {
                    on.push(STATE_FOCUSED);
                }
                bits(&on)
            }
            Target::Cell(e, at, col) => {
                let mut on = vec![STATE_VISIBLE];
                if !snap.rows_behind() {
                    on.extend([STATE_ENABLED, STATE_SENSITIVE]);
                }
                // Selected with its row, as `Table.IsSelected` says.
                if snap.rows.is_selected(*e) {
                    on.push(STATE_SELECTED);
                }
                if snap.rows.showing(*at) && snap.rows.columns.get(*col).is_some_and(|c| c.w > 0) {
                    on.push(STATE_SHOWING);
                }
                bits(&on)
            }
        }
    }

    fn rect(&self, target: &Target<'_>, snap: &Snapshot) -> Option<crate::canvas::Rect> {
        match target {
            Target::Root => None,
            Target::Fixed(n) => Some(n.rect),
            Target::Row(_, at) => Some(snap.rows.rect(*at)),
            Target::Cell(_, at, col) => Some(snap.rows.cell_rect(*at, *col)),
        }
    }

    fn is_place(n: &Node) -> bool {
        n.parent == a11y::SIDEBAR && n.role == Role::ListItem
    }

    /// The single action an object has.
    fn action_of(&self, target: &Target<'_>) -> Option<a11y::Action> {
        match target {
            Target::Fixed(n) => n.action,
            _ => None,
        }
    }

    fn props(&self, target: &Target<'_>, snap: &Snapshot, iface: &str) -> Vec<(&'static str, Variant<'static>)> {
        match (iface, target) {
            (ACCESSIBLE, _) => vec![
                ("Name", Variant::from(self.name(target, snap))),
                ("Description", Variant::from(self.description(target, snap))),
                ("Parent", Variant::from(self.parent(target, snap))),
                ("ChildCount", Variant::from(self.child_count(target, snap) as i32)),
                ("Locale", Variant::from(String::new())),
                ("AccessibleId", Variant::from(String::new())),
                ("HelpText", Variant::from(String::new())),
            ],
            (APPLICATION, Target::Root) => vec![
                ("ToolkitName", Variant::from("Gannet".to_string())),
                ("Version", Variant::from(env!("CARGO_PKG_VERSION").to_string())),
                ("AtspiVersion", Variant::from("2.1".to_string())),
                ("Id", Variant::from(*self.id.lock().unwrap())),
            ],
            (ACTION, _) if self.action_of(target).is_some() => vec![("NActions", Variant::from(1i32))],
            (SELECTION, Target::Fixed(n)) if Self::selects(n) => vec![("NSelectedChildren", Variant::from(self.selected_children(n, snap).len() as i32))],
            (TABLE, Target::Fixed(n)) if n.id == a11y::LIST => vec![
                ("NRows", Variant::from(snap.rows.len() as i32)),
                ("NColumns", Variant::from(snap.rows.columns.len() as i32)),
                ("Caption", Variant::from(null())),
                ("Summary", Variant::from(null())),
                ("NSelectedRows", Variant::from(self.selected_children(n, snap).len() as i32)),
                ("NSelectedColumns", Variant::from(0i32)),
            ],
            (TABLE_CELL, Target::Cell(_, at, col)) => vec![
                ("ColumnSpan", Variant::from(1i32)),
                ("RowSpan", Variant::from(1i32)),
                ("Position", Variant::from((*at as i32, *col as i32))),
                ("Table", Variant::from(self.fixed_ref(a11y::LIST))),
            ],
            (VALUE, Target::Fixed(n)) if n.role == Role::ProgressBar => {
                let job = snap.news.jobs.iter().find(|j| a11y::JOB0 + j.id == n.id);
                vec![
                    ("MinimumValue", Variant::from(0.0f64)),
                    ("MaximumValue", Variant::from(100.0f64)),
                    ("MinimumIncrement", Variant::from(1.0f64)),
                    ("CurrentValue", Variant::from(job.and_then(|j| j.percent).map_or(0.0, f64::from))),
                    ("Text", Variant::from(job.map(|j| j.progress.clone()).unwrap_or_default())),
                ]
            }
            (TEXT, Target::Fixed(Node { field: Some(f), .. })) => vec![("CharacterCount", Variant::from(f.text.chars().count() as i32)), ("CaretOffset", Variant::from(f.chars_before(f.caret) as i32))],
            _ => Vec::new(),
        }
    }

    fn accessible(&self, h: &Header<'_>, m: &str, body: &Body, target: &Target<'_>, snap: &Snapshot) {
        match m {
            "GetChildAtIndex" => {
                let i: i32 = body.deserialize().unwrap_or(-1);
                self.reply(h, &(usize::try_from(i).ok().and_then(|i| self.child(target, snap, i)).unwrap_or_else(null),))
            }
            "GetChildren" => {
                let kids: Vec<Ref> = (0..self.child_count(target, snap)).filter_map(|i| self.child(target, snap, i)).collect();
                self.reply(h, &(kids,))
            }
            "GetIndexInParent" => self.reply(h, &(self.index_in_parent(target, snap),)),
            "GetRelationSet" => self.reply(h, &(Vec::<(u32, Vec<Ref>)>::new(),)),
            "GetRole" => self.reply(h, &(self.role(target).0,)),
            "GetRoleName" | "GetLocalizedRoleName" => self.reply(h, &(self.role(target).1,)),
            "GetState" => self.reply(h, &(self.states(target, snap),)),
            "GetAttributes" => self.reply(h, &(self.attributes(target, snap),)),
            "GetApplication" => self.reply(h, &(self.at(ROOT),)),
            "GetInterfaces" => {
                let mut v = vec![ACCESSIBLE];
                match target {
                    Target::Root => v.push(APPLICATION),
                    _ => v.push(COMPONENT),
                }
                if self.action_of(target).is_some() {
                    v.push(ACTION);
                }
                match target {
                    Target::Fixed(n) if Self::selects(n) => v.push(SELECTION),
                    Target::Cell(..) => v.push(TABLE_CELL),
                    _ => {}
                }
                match target {
                    Target::Fixed(n) if n.id == a11y::LIST => v.push(TABLE),
                    Target::Fixed(n) if n.role == Role::ProgressBar => v.push(VALUE),
                    _ => {}
                }
                if matches!(target, Target::Fixed(n) if n.field.is_some()) {
                    v.extend([TEXT, EDITABLE_TEXT]);
                }
                self.reply(h, &(v,))
            }
            _ => self.error(h, UNKNOWN_METHOD, m),
        }
    }

    fn component(&self, h: &Header<'_>, m: &str, body: &Body, target: &Target<'_>, snap: &Snapshot) {
        let Some(r) = self.rect(target, snap) else { return self.error(h, UNKNOWN_METHOD, m) };
        // Wayland never says where the window is, so every kind of coordinate is the window's.
        match m {
            "GetExtents" => self.reply(h, &((r.x, r.y, r.w, r.h),)),
            "GetPosition" => self.reply(h, &(r.x, r.y)),
            "GetSize" => self.reply(h, &(r.w, r.h)),
            "GetLayer" => self.reply(h, &(if matches!(target, Target::Fixed(n) if matches!(n.role, Role::Frame | Role::Dialog)) { 7u32 } else { 3u32 },)),
            "GetMDIZOrder" => self.reply(h, &(0i16,)),
            "GetAlpha" => self.reply(h, &(1.0f64,)),
            "Contains" => {
                let (x, y, _): (i32, i32, u32) = body.deserialize().unwrap_or((-1, -1, 0));
                self.reply(h, &(r.contains(x, y),))
            }
            "GetAccessibleAtPoint" => {
                let (x, y, _): (i32, i32, u32) = body.deserialize().unwrap_or((-1, -1, 0));
                let hit = match target {
                    Target::Fixed(n) if n.id == a11y::LIST => (r.contains(x, y) && snap.rows.row_h > 0)
                        .then(|| ((y - r.y) as f64 + snap.rows.scroll) / snap.rows.row_h as f64)
                        .and_then(|at| snap.rows.entry(at as usize))
                        .map(|e| self.row_ref(&snap.rows, e)),
                    Target::Row(e, at) => (0..snap.rows.columns.len()).find(|&c| snap.rows.cell_rect(*at, c).contains(x, y)).map(|c| self.cell_ref(&snap.rows, *e, c)),
                    // A place scrolled out of the sidebar isn't there to point at. The last child
                    // drawn is on top: the rename field over the list.
                    Target::Fixed(n) => n.children.iter().rev().filter_map(|&c| snap.get(c)).find(|c| c.showing && r.contains(x, y) && c.rect.contains(x, y)).map(|c| self.fixed_ref(c.id)),
                    _ => None,
                };
                self.reply(h, &(hit.unwrap_or_else(null),))
            }
            "GrabFocus" => {
                let ok = match target {
                    // A cell moved to puts the cursor on its row, as clicking it does.
                    Target::Row(e, _) | Target::Cell(e, ..) => self.tell_row(snap, *e, Act::FocusRow),
                    // The keyboard doesn't go to the sidebar, but a place a screen reader moves
                    // to is shown.
                    Target::Fixed(n) if Self::is_place(n) => self.show_place(n, snap),
                    // A part of the menu or dialog that's open, which moving to picks, as the
                    // keyboard or pointer does.
                    Target::Fixed(n) if snap.modal.is_some() && n.enabled && matches!(n.role, Role::MenuItem | Role::PushButton | Role::Entry) => {
                        self.hub.send(Msg::A11y(Act::Focus(n.id)));
                        true
                    }
                    // An entry, or an item of another list, which moving to picks.
                    Target::Fixed(n) if matches!(n.role, Role::Entry | Role::ListItem) && n.enabled && n.parent != a11y::RENAME_NAMES => {
                        self.hub.send(Msg::A11y(Act::Focus(n.id)));
                        true
                    }
                    _ => false,
                };
                self.reply(h, &(ok,))
            }
            "ScrollTo" | "ScrollToPoint" => {
                let ok = match target {
                    Target::Row(e, _) | Target::Cell(e, ..) => self.tell_row(snap, *e, Act::ShowRow),
                    Target::Fixed(n) if Self::is_place(n) => self.show_place(n, snap),
                    _ => false,
                };
                self.reply(h, &(ok,))
            }
            "SetExtents" | "SetPosition" | "SetSize" => self.reply(h, &(false,)),
            _ => self.error(h, UNKNOWN_METHOD, m),
        }
    }

    /// A place's index among the sidebar's places, which leaves out its headings.
    fn place_index(n: &Node, snap: &Snapshot) -> Option<usize> {
        let _ = snap.get(a11y::SIDEBAR)?.children.iter().position(|&c| c == n.id)?;
        usize::try_from(n.id.checked_sub(a11y::PLACE0)?).ok()
    }

    fn show_place(&self, n: &Node, snap: &Snapshot) -> bool {
        Self::place_index(n, snap).is_some_and(|i| {
            self.hub.send(Msg::A11y(Act::ShowPlace(i)));
            true
        })
    }

    /// Asks the app to do something to a row, named by its folder and key.
    fn tell_row(&self, snap: &Snapshot, entry: u32, act: impl FnOnce(Arc<std::path::PathBuf>, Vec<u8>) -> Act) -> bool {
        self.hub.send(Msg::A11y(act(snap.dir.clone(), snap.rows.key(entry).into_owned())));
        true
    }

    fn action(&self, h: &Header<'_>, m: &str, body: &Body, target: &Target<'_>, snap: &Snapshot) {
        let Some(a) = self.action_of(target) else { return self.error(h, UNKNOWN_METHOD, m) };
        let Target::Fixed(n) = target else { return self.error(h, UNKNOWN_METHOD, m) };
        match m {
            "GetActions" => self.reply(h, &(vec![(a.name, a.description, a.keys)],)),
            "GetName" | "GetLocalizedName" => self.reply(h, &(a.name,)),
            "GetDescription" => self.reply(h, &(a.description,)),
            "GetKeyBinding" => self.reply(h, &(a.keys,)),
            "DoAction" => {
                let i: i32 = body.deserialize().unwrap_or(-1);
                // A clash just asked takes no answer for a moment (`App::dialog_guarded`). Its
                // buttons aren't shown as unavailable meanwhile, but a press is refused.
                let guarded = n.parent == a11y::DIALOG && snap.modal.and_then(|m| m.asked).is_some_and(|t| t.elapsed() < crate::app::DIALOG_GUARD);
                let ok = i == 0 && n.enabled && !guarded;
                if ok {
                    let act = if Self::is_place(n) {
                        Act::OpenPlace(Self::place_index(n, snap).unwrap_or(usize::MAX))
                    } else if matches!(n.role, Role::MenuItem) || (n.parent == a11y::DIALOG && n.role == Role::PushButton) {
                        // Named, so it can't press another menu's or dialog's in its place.
                        Act::Choose(n.id, n.name.clone())
                    } else if n.parent == a11y::PATH {
                        Act::OpenCrumb(snap.dir.clone(), (n.id - a11y::CRUMB0) as usize)
                    } else if (a11y::EJECT0..a11y::NOTICE0).contains(&n.id) {
                        // Named for the drive, so it can't eject another that took its place.
                        Act::Eject((n.id - a11y::EJECT0) as usize, snap.get(n.parent).map(|p| p.name.clone()).unwrap_or_default())
                    } else if (a11y::DISMISS0..a11y::DISMISS0 * 2).contains(&n.id) {
                        Act::Dismiss((n.id - a11y::DISMISS0) as usize, n.description.clone())
                    } else {
                        Act::Press(n.id)
                    };
                    self.hub.send(Msg::A11y(act));
                }
                self.reply(h, &(ok,))
            }
            _ => self.error(h, UNKNOWN_METHOD, m),
        }
    }

    /// A list's selected children, in order, as places among its children.
    fn selected_children(&self, n: &Node, snap: &Snapshot) -> Vec<usize> {
        if n.id == a11y::LIST {
            let r = &snap.rows;
            (0..r.len()).filter(|&at| r.entry(at).is_some_and(|e| r.is_selected(e))).collect()
        } else {
            n.children.iter().enumerate().filter(|(_, c)| snap.get(**c).is_some_and(|c| c.selected)).map(|(i, _)| i).collect()
        }
    }

    /// Whether a list picks among its children through Selection. The path bar's links and the
    /// rename dialog's names don't.
    fn selects(n: &Node) -> bool {
        matches!(n.role, Role::List | Role::Table) && n.id != a11y::PATH && n.id != a11y::RENAME_NAMES
    }

    /// A column heading's sort order, as Orca reads it, the folder shown as the current part of
    /// the path, and the sidebar's headings' level.
    fn attributes(&self, target: &Target<'_>, snap: &Snapshot) -> HashMap<&'static str, &'static str> {
        let mut a = HashMap::from([("toolkit", "Gannet")]);
        match target {
            Target::Fixed(n) if n.role == Role::ColumnHeader => {
                let (column, descending) = snap.rows.sort;
                a.insert("sort", if column.heading() != n.id { "none" } else if descending { "descending" } else { "ascending" });
            }
            Target::Fixed(n) if n.role == Role::Link && n.selected => {
                a.insert("current", "location");
            }
            Target::Fixed(n) if n.role == Role::Heading => {
                a.insert("level", "2");
            }
            _ => {}
        }
        a
    }

    /// The file list as a table: a row for each file, and a column for each of the list's.
    /// A cell's index is its row's place times the number of columns, plus its column's.
    fn table(&self, h: &Header<'_>, m: &str, body: &Body, target: &Target<'_>, snap: &Snapshot) {
        let Target::Fixed(n) = target else { return self.error(h, UNKNOWN_METHOD, m) };
        if n.id != a11y::LIST {
            return self.error(h, UNKNOWN_METHOD, m);
        }
        let rows = &snap.rows;
        let cols = rows.columns.len();
        let cell = |(r, c): (i32, i32)| -> Option<(u32, usize, usize)> {
            let (r, c) = (usize::try_from(r).ok()?, usize::try_from(c).ok().filter(|&c| c < cols)?);
            Some((rows.entry(r)?, r, c))
        };
        let at = |i: i32| usize::try_from(i).ok().filter(|_| cols > 0).and_then(|i| cell(((i / cols) as i32, (i % cols) as i32)));
        let two = || body.deserialize::<(i32, i32)>().unwrap_or((-1, -1));
        let one = || body.deserialize::<i32>().unwrap_or(-1);
        match m {
            "GetAccessibleAt" => self.reply(h, &(cell(two()).map_or_else(null, |(e, _, c)| self.cell_ref(rows, e, c)),)),
            "GetIndexAt" => self.reply(h, &(cell(two()).map_or(-1, |(_, r, c)| (r * cols + c) as i32),)),
            "GetRowAtIndex" => self.reply(h, &(at(one()).map_or(-1, |(_, r, _)| r as i32),)),
            "GetColumnAtIndex" => self.reply(h, &(at(one()).map_or(-1, |(_, _, c)| c as i32),)),
            "GetRowDescription" => self.reply(h, &("",)),
            "GetColumnDescription" => self.reply(h, &(usize::try_from(one()).ok().and_then(|c| rows.columns.get(c)).map_or("", |c| c.name),)),
            "GetRowExtentAt" | "GetColumnExtentAt" => self.reply(h, &(i32::from(cell(two()).is_some()),)),
            "GetRowHeader" => self.reply(h, &(null(),)),
            "GetColumnHeader" => self.reply(h, &(usize::try_from(one()).ok().and_then(|c| rows.columns.get(c)).map_or_else(null, |c| self.fixed_ref(c.column.heading())),)),
            "GetSelectedRows" => self.reply(h, &(self.selected_children(n, snap).into_iter().map(|r| r as i32).collect::<Vec<i32>>(),)),
            "GetSelectedColumns" => self.reply(h, &(Vec::<i32>::new(),)),
            "IsRowSelected" => self.reply(h, &(usize::try_from(one()).ok().and_then(|r| rows.entry(r)).is_some_and(|e| rows.is_selected(e)),)),
            "IsSelected" => self.reply(h, &(cell(two()).is_some_and(|(e, ..)| rows.is_selected(e)),)),
            "IsColumnSelected" | "AddColumnSelection" | "RemoveColumnSelection" => self.reply(h, &(false,)),
            // As through Selection: adding a row leaves the rest of the selection as it is.
            "AddRowSelection" => self.reply(h, &(usize::try_from(one()).ok().and_then(|r| rows.entry(r)).is_some_and(|e| self.tell_row(snap, e, |d, k| Act::SelectRow(d, k, true))),)),
            "RemoveRowSelection" => self.reply(h, &(usize::try_from(one()).ok().and_then(|r| rows.entry(r)).is_some_and(|e| rows.is_selected(e) && self.tell_row(snap, e, |d, k| Act::SelectRow(d, k, false))),)),
            "GetRowColumnExtentsAtIndex" => match at(one()) {
                Some((e, r, c)) => self.reply(h, &(true, r as i32, c as i32, 1i32, 1i32, rows.is_selected(e))),
                None => self.reply(h, &(false, -1i32, -1i32, 0i32, 0i32, false)),
            },
            _ => self.error(h, UNKNOWN_METHOD, m),
        }
    }

    /// A row's cell, under its column's heading.
    fn table_cell(&self, h: &Header<'_>, m: &str, target: &Target<'_>, snap: &Snapshot) {
        let Target::Cell(_, at, col) = target else { return self.error(h, UNKNOWN_METHOD, m) };
        match m {
            "GetRowColumnSpan" => self.reply(h, &(true, *at as i32, *col as i32, 1i32, 1i32)),
            "GetColumnHeaderCells" => self.reply(h, &(snap.rows.columns.get(*col).map(|c| self.fixed_ref(c.column.heading())).into_iter().collect::<Vec<Ref>>(),)),
            "GetRowHeaderCells" => self.reply(h, &(Vec::<Ref>::new(),)),
            _ => self.error(h, UNKNOWN_METHOD, m),
        }
    }

    fn selection(&self, h: &Header<'_>, m: &str, body: &Body, target: &Target<'_>, snap: &Snapshot) {
        let Target::Fixed(n) = target else { return self.error(h, UNKNOWN_METHOD, m) };
        if !Self::selects(n) {
            return self.error(h, UNKNOWN_METHOD, m);
        }
        let i = usize::try_from(body.deserialize::<i32>().unwrap_or(-1)).ok();
        match m {
            "GetSelectedChild" => {
                let at = i.and_then(|i| self.selected_children(n, snap).get(i).copied());
                self.reply(h, &(at.and_then(|at| self.child(target, snap, at)).unwrap_or_else(null),))
            }
            "IsChildSelected" => {
                let on = match i {
                    Some(at) if n.id == a11y::LIST => snap.rows.entry(at).is_some_and(|e| snap.rows.is_selected(e)),
                    Some(at) => n.children.get(at).and_then(|&c| snap.get(c)).is_some_and(|c| c.selected),
                    None => false,
                };
                self.reply(h, &(on,))
            }
            "SelectChild" => {
                // Selecting a row adds it to the selection, as Ctrl+click does, leaving the
                // rest and the cursor as they are; selecting a place opens it, as clicking one
                // does; and in the other lists, which pick one thing, it picks it.
                let ok = match i {
                    Some(at) if n.id == a11y::LIST => snap.rows.entry(at).is_some_and(|e| self.tell_row(snap, e, |d, k| Act::SelectRow(d, k, true))),
                    Some(at) if n.id == a11y::SIDEBAR => n.children.get(at).and_then(|&c| snap.get(c)).filter(|c| Self::is_place(c)).and_then(|c| Self::place_index(c, snap)).is_some_and(|i| {
                        self.hub.send(Msg::A11y(Act::OpenPlace(i)));
                        true
                    }),
                    Some(at) => n.children.get(at).and_then(|&c| snap.get(c)).filter(|c| c.role == Role::ListItem && c.enabled).is_some_and(|c| {
                        self.hub.send(Msg::A11y(Act::Focus(c.id)));
                        true
                    }),
                    _ => false,
                };
                self.reply(h, &(ok,))
            }
            "DeselectChild" | "DeselectSelectedChild" if n.id == a11y::LIST => {
                // One counts among all the rows, the other among the selected ones.
                let at = if m == "DeselectChild" { i } else { i.and_then(|i| self.selected_children(n, snap).get(i).copied()) };
                let ok = at.and_then(|at| snap.rows.entry(at)).is_some_and(|e| snap.rows.is_selected(e) && self.tell_row(snap, e, |d, k| Act::SelectRow(d, k, false)));
                self.reply(h, &(ok,))
            }
            "DeselectSelectedChild" | "DeselectChild" | "SelectAll" | "ClearSelection" => self.reply(h, &(false,)),
            _ => self.error(h, UNKNOWN_METHOD, m),
        }
    }

    /// Asks the app to change a field, through what typing in it does.
    fn edit_text(&self, n: &Node, snap: &Snapshot, op: TextEdit) -> bool {
        self.hub.send(Msg::A11y(Act::EditText(n.id, snap.edit_gen, op)));
        true
    }

    /// A field's Text interface: reading it, and moving its caret and selection.
    fn text(&self, h: &Header<'_>, m: &str, body: &Body, target: &Target<'_>, snap: &Snapshot) {
        let Target::Fixed(n @ Node { field: Some(f), .. }) = target else { return self.error(h, UNKNOWN_METHOD, m) };
        let chars: Vec<char> = f.text.chars().collect();
        let len = chars.len();
        let at = |i: i32| usize::try_from(i).unwrap_or(0).min(len);
        let part = |a: usize, b: usize| chars[a.min(b)..b].iter().collect::<String>();
        let caret = f.chars_before(f.caret);
        let (sa, sb) = f.selection();
        let (sa, sb) = (f.chars_before(sa), f.chars_before(sb));
        let geometry = snap.geometry(n.id);
        let extents = |a: usize, b: usize| match geometry {
            // An empty range is where its place is, as wide as nothing.
            Some(g) => {
                let (first, last) = (g.char_rect(a), g.char_rect(b.max(a + 1) - 1));
                let right = if b > a { last.x + last.w } else { first.x };
                (first.x, first.y, right - first.x, first.h)
            }
            None => (0, 0, 0, 0),
        };
        match m {
            "GetText" => {
                let (a, b): (i32, i32) = body.deserialize().unwrap_or((0, -1));
                let b = if b < 0 { len } else { at(b) };
                self.reply(h, &(part(at(a).min(b), b),))
            }
            "GetStringAtOffset" | "GetTextAtOffset" | "GetTextBeforeOffset" | "GetTextAfterOffset" => {
                let (i, kind): (i32, u32) = body.deserialize().unwrap_or((0, 0));
                // The newer call counts characters, words, sentences, lines and paragraphs;
                // the older ones characters, then the starts and ends of each of the rest.
                let unit = match kind {
                    0 => Unit::Char,
                    1 => Unit::Word,
                    2 if m != "GetStringAtOffset" => Unit::Word,
                    // A field is one line, and one sentence as far as anyone can tell.
                    _ => Unit::Line,
                };
                let (a, b) = match m {
                    "GetTextBeforeOffset" => before(&chars, at(i), unit),
                    "GetTextAfterOffset" => after(&chars, at(i), unit),
                    _ => segment(&chars, at(i), unit),
                };
                self.reply(h, &(part(a, b), a as i32, b as i32))
            }
            "GetCharacterAtOffset" => {
                let i: i32 = body.deserialize().unwrap_or(-1);
                self.reply(h, &(usize::try_from(i).ok().and_then(|i| chars.get(i)).map_or(0, |&c| c as i32),))
            }
            "SetCaretOffset" => {
                let i: i32 = body.deserialize().unwrap_or(0);
                self.reply(h, &(self.edit_text(n, snap, TextEdit::Caret(at(i))),))
            }
            "GetNSelections" => self.reply(h, &(f.has_selection() as i32,)),
            "GetSelection" => {
                let i: i32 = body.deserialize().unwrap_or(-1);
                self.reply(h, &(if i == 0 && f.has_selection() { (sa as i32, sb as i32) } else { (0, 0) }))
            }
            "AddSelection" => {
                // A field has one selection at most.
                let (a, b): (i32, i32) = body.deserialize().unwrap_or((0, 0));
                let ok = !f.has_selection() && self.edit_text(n, snap, TextEdit::Select(at(a), at(b)));
                self.reply(h, &(ok,))
            }
            "SetSelection" => {
                let (i, a, b): (i32, i32, i32) = body.deserialize().unwrap_or((-1, 0, 0));
                let ok = i == 0 && self.edit_text(n, snap, TextEdit::Select(at(a), at(b)));
                self.reply(h, &(ok,))
            }
            "RemoveSelection" => {
                let i: i32 = body.deserialize().unwrap_or(-1);
                let ok = i == 0 && f.has_selection() && self.edit_text(n, snap, TextEdit::Caret(caret));
                self.reply(h, &(ok,))
            }
            "GetCharacterExtents" => {
                let (i, _): (i32, u32) = body.deserialize().unwrap_or((0, 0));
                let a = at(i);
                self.reply(h, &extents(a, (a + 1).min(len).max(a)))
            }
            "GetRangeExtents" => {
                let (a, b, _): (i32, i32, u32) = body.deserialize().unwrap_or((0, 0, 0));
                let (a, b) = (at(a.min(b)), at(a.max(b)));
                self.reply(h, &extents(a, b))
            }
            "GetOffsetAtPoint" => {
                let (x, y, _): (i32, i32, u32) = body.deserialize().unwrap_or((-1, -1, 0));
                self.reply(h, &(geometry.and_then(|g| g.char_at(x, y)).map_or(-1, |i| i as i32),))
            }
            // Every character looks the same, as far as anything here says.
            "GetAttributes" | "GetAttributeRun" => self.reply(h, &(HashMap::<String, String>::new(), 0i32, len as i32)),
            "GetDefaultAttributes" | "GetDefaultAttributeSet" => self.reply(h, &(HashMap::<String, String>::new(),)),
            "GetAttributeValue" => self.reply(h, &("",)),
            "GetBoundedRanges" => self.reply(h, &(Vec::<(i32, i32, String, Variant<'_>)>::new(),)),
            "ScrollSubstringTo" | "ScrollSubstringToPoint" => self.reply(h, &(false,)),
            _ => self.error(h, UNKNOWN_METHOD, m),
        }
    }

    /// A field's EditableText interface: each change is made as typing it would be.
    fn editable_text(&self, h: &Header<'_>, m: &str, body: &Body, target: &Target<'_>, snap: &Snapshot) {
        let Target::Fixed(n @ Node { field: Some(f), .. }) = target else { return self.error(h, UNKNOWN_METHOD, m) };
        let len = f.text.chars().count();
        let at = |i: i32| usize::try_from(i).unwrap_or(0).min(len);
        let range = |(a, b): (i32, i32)| {
            let b = if b < 0 { len } else { at(b) };
            (at(a).min(b), b)
        };
        match m {
            "SetTextContents" => {
                // Not read as empty, which would clear the field.
                let Ok(text) = body.deserialize::<String>() else { return self.error(h, "org.freedesktop.DBus.Error.InvalidArgs", m) };
                // To the end of the text as it is then, whatever's been typed since this snapshot.
                self.reply(h, &(self.edit_text(n, snap, TextEdit::Replace(0, usize::MAX, text)),))
            }
            "InsertText" => {
                let (i, mut text, bytes): (i32, String, i32) = body.deserialize().unwrap_or_default();
                // The length is in bytes, and only cuts the text short.
                if let Ok(bytes) = usize::try_from(bytes)
                    && bytes < text.len()
                {
                    let mut end = bytes;
                    while !text.is_char_boundary(end) {
                        end -= 1;
                    }
                    text.truncate(end);
                }
                self.reply(h, &(self.edit_text(n, snap, TextEdit::Replace(at(i), at(i), text)),))
            }
            "CopyText" => {
                let (a, b) = range(body.deserialize().unwrap_or((0, 0)));
                self.edit_text(n, snap, TextEdit::Copy(a, b));
                self.reply(h, &())
            }
            "CutText" => {
                let (a, b) = range(body.deserialize().unwrap_or((0, 0)));
                self.reply(h, &(self.edit_text(n, snap, TextEdit::Cut(a, b)),))
            }
            "DeleteText" => {
                let (a, b) = range(body.deserialize().unwrap_or((0, 0)));
                self.reply(h, &(self.edit_text(n, snap, TextEdit::Replace(a, b, String::new())),))
            }
            "PasteText" => {
                let Ok(i) = body.deserialize::<i32>() else { return self.error(h, "org.freedesktop.DBus.Error.InvalidArgs", m) };
                self.reply(h, &(self.edit_text(n, snap, TextEdit::Paste(at(i))),))
            }
            _ => self.error(h, UNKNOWN_METHOD, m),
        }
    }

    /// Says how a field's text changed, then where its caret went and what's selected, so a
    /// screen reader reads what was typed or deleted and what the caret moved over.
    fn emit_text_changes(&self, path: &str, old: &Field, new: &Field) {
        if old.text != new.text {
            let (at, gone, came) = text_change(&old.text, &new.text);
            if !gone.is_empty() {
                self.event2(path, OBJECT_EVENT, "TextChanged", ("delete", at as i32, gone.chars().count() as i32), Variant::from(gone));
            }
            if !came.is_empty() {
                self.event2(path, OBJECT_EVENT, "TextChanged", ("insert", at as i32, came.chars().count() as i32), Variant::from(came));
            }
        }
        let caret = |f: &Field| f.chars_before(f.caret);
        if caret(old) != caret(new) {
            self.event(path, OBJECT_EVENT, "TextCaretMoved", ("", caret(new) as i32), Variant::from(0i32));
        }
        let selected = |f: &Field| {
            let (a, b) = f.selection();
            (a != b).then(|| (f.chars_before(a), f.chars_before(b)))
        };
        if selected(old) != selected(new) {
            self.event(path, OBJECT_EVENT, "TextSelectionChanged", ("", 0), Variant::from(0i32));
        }
    }

    /// Tells listeners what changed between two snapshots.
    fn emit_changes(&self, old: &Snapshot, new: &Snapshot) {
        let state = |path: &str, what: &str, on: bool| self.event(path, OBJECT_EVENT, "StateChanged", (what, on as i32), Variant::from(0i32));
        // What had the keyboard, by its path before any part becomes a new object below.
        let path_of = |s: &Snapshot| match s.focus {
            a11y::Focus::Node(id) => self.path(id),
            a11y::Focus::Row(e) => row_path(s.rows.era, &s.rows.key(e)),
        };
        let had = old.window_focused.then(|| path_of(old));
        let old_list = self.fixed_ref(a11y::LIST);
        // A part back under an id that another part had is a new object. A dialog's parts take
        // the ids the last dialog's had, and a screen reader that kept what those said read a
        // new alert under the last dialog's name, and its line twice. The list is a new table in
        // each folder: a screen reader keeps how many rows and columns a table has until it
        // hears the table change, which it hears only after reading where the keyboard went,
        // so it read the last folder's count.
        let new_list = old.rows.era != new.rows.era && old.get(a11y::LIST).is_some() && new.get(a11y::LIST).is_some();
        {
            let served = self.served.lock().unwrap();
            let mut incarnations = self.incarnations.lock().unwrap();
            for n in new.fixed.iter().filter(|n| old.get(n.id).is_none()) {
                if served.get(&n.id).is_some_and(|(role, name, description)| (*role, name, description) != (n.role, &n.name, &n.description)) {
                    *incarnations.entry(n.id).or_default() += 1;
                }
            }
            if new_list {
                *incarnations.entry(a11y::LIST).or_default() += 1;
            }
        }
        let frame = self.path(a11y::FRAME);
        if old.window_focused != new.window_focused {
            let member = if new.window_focused { "Activate" } else { "Deactivate" };
            self.event(&frame, WINDOW_EVENT, member, ("", 0), Variant::from(""));
            state(&frame, "active", new.window_focused);
        }

        // The fixed parts: names, whether they can be used, and the sidebar's places.
        // The rename dialog has a part for each name, so parts are looked up by id.
        let olds: HashMap<u64, &Node> = old.fixed.iter().map(|n| (n.id, n)).collect();
        let news: HashMap<u64, &Node> = new.fixed.iter().map(|n| (n.id, n)).collect();
        let focused = |s: &Snapshot, id: u64| s.window_focused && s.focus == a11y::Focus::Node(id);
        for n in &new.fixed {
            let Some(o) = olds.get(&n.id).copied() else { continue };
            let path = self.path(n.id);
            // A new folder's table is a new object, with nothing of the last one's to change: an
            // empty folder's "This folder is empty" was read twice.
            if n.id == a11y::LIST && new_list {
                continue;
            }
            if o.name != n.name {
                self.event(&path, OBJECT_EVENT, "PropertyChange", ("accessible-name", 0), Variant::from(n.name.clone()));
            }
            // The filter box turning into a search, and back, among others. The unlock prompt's
            // line is announced instead (`emit_dialog`): as a description change too, it would
            // be read twice, and a screen reader that reads the changes in turn can end on an
            // "Unlocking…" that's already over.
            if o.description != n.description && !(n.id == a11y::DIALOG && new.get(a11y::PASSPHRASE).is_some()) {
                self.event(&path, OBJECT_EVENT, "PropertyChange", ("accessible-description", 0), Variant::from(n.description.clone()));
            }
            if o.checked != n.checked {
                state(&path, "checked", n.checked == Some(true));
                // A screen reader reads that only of the part with the keyboard, and a box is
                // often ticked from elsewhere: Space in Open With's list, A in a clash.
                if !focused(new, n.id) && o.checked.is_some() && n.checked.is_some() {
                    let said = format!("{}, {}", n.name, if n.checked == Some(true) { "checked" } else { "not checked" });
                    self.event(&frame, OBJECT_EVENT, "Announcement", ("", POLITE), Variant::from(said));
                }
            }
            if let (Some(of), Some(nf)) = (&o.field, &n.field) {
                // Only within one field: a rename finished and another started in between is a
                // new field under the same id, not an edit of the last.
                if old.edit_gen == new.edit_gen {
                    self.emit_text_changes(&path, of, nf);
                }
            }
            // Whether it can be used: a menu or dialog opening over it, or closing, changes that.
            let (was, is) = (o.enabled && !old.behind_in(n.id, |id| olds.get(&id).copied()), n.enabled && !new.behind_in(n.id, |id| news.get(&id).copied()));
            if was != is {
                state(&path, "enabled", is);
                state(&path, "sensitive", is);
            }
            // A place scrolling into or out of the sidebar.
            if o.showing != n.showing {
                state(&path, "showing", n.showing);
            }
            // A place is "selected" while the window is in it, which only marks it: announcing
            // that would read out the sidebar each time the folder changes, while the keyboard
            // is in the list. A screen reader asking still hears it. The folder shown is the
            // current part of the path, which isn't a selection either.
            if o.selected != n.selected && n.parent != a11y::SIDEBAR && n.role != Role::Link {
                state(&path, "selected", n.selected);
                if n.role == Role::ListItem {
                    self.event(&self.path(n.parent), OBJECT_EVENT, "SelectionChanged", ("", 0), Variant::from(0i32));
                }
            }
        }
        // Parts coming and going: places, the fields in the frame, and the Open With window's
        // apps as a search narrows them.
        for n in &new.fixed {
            let Some(o) = olds.get(&n.id).copied().filter(|o| o.children != n.children) else { continue };
            let path = self.path(n.id);
            for (i, id) in o.children.iter().enumerate().rev().filter(|(_, id)| !n.children.contains(id)) {
                self.event(&path, OBJECT_EVENT, "ChildrenChanged", ("remove", i as i32), Variant::from(self.fixed_ref(*id)));
            }
            for (i, id) in n.children.iter().enumerate().filter(|(_, id)| !o.children.contains(id)) {
                self.event(&path, OBJECT_EVENT, "ChildrenChanged", ("add", i as i32), Variant::from(self.fixed_ref(*id)));
            }
        }
        // The last folder's table gives way to this one's.
        if new_list && let Some(list) = new.get(a11y::LIST) {
            let at = new.get(list.parent).and_then(|p| p.children.iter().position(|&c| c == a11y::LIST)).map_or(-1, |at| at as i32);
            let parent = self.path(list.parent);
            self.event(&parent, OBJECT_EVENT, "ChildrenChanged", ("remove", at), Variant::from(old_list));
            self.event(&parent, OBJECT_EVENT, "ChildrenChanged", ("add", at), Variant::from(self.fixed_ref(a11y::LIST)));
        }

        // The rows on show can't be used while a menu or dialog is open over them. A screen
        // reader asks again about any other.
        let list = self.path(a11y::LIST);
        let same_folder = old.rows.era == new.rows.era;
        if old.rows_behind() != new.rows_behind() {
            let r = &new.rows;
            for e in (0..r.len()).skip_while(|&at| !r.showing(at)).take_while(|&at| r.showing(at)).filter_map(|at| r.entry(at)) {
                let path = row_path(r.era, &r.key(e));
                state(&path, "enabled", !new.rows_behind());
                state(&path, "sensitive", !new.rows_behind());
            }
        }

        // The rows. Within one folder, a few rows coming or going are announced one by one.
        // Otherwise the table is said to have been reordered: a screen reader keeps how many
        // rows and columns a table has until it hears the table change, and would go on
        // reading the last folder's.
        let columns = |s: &Snapshot| s.rows.columns.iter().map(|c| c.column).collect::<Vec<_>>();
        let mut reordered = same_folder && columns(old) != columns(new);
        if same_folder && !Arc::ptr_eq(&old.rows.order, &new.rows.order) {
            let gone: Vec<(usize, u32)> = old.rows.order.iter().enumerate().filter(|(_, e)| new.rows.position_of(&old.rows.key(**e)).is_none()).map(|(at, e)| (at, *e)).take(ROW_EVENTS + 1).collect();
            let came: Vec<(usize, u32)> = new.rows.order.iter().enumerate().filter(|(_, e)| old.rows.position_of(&new.rows.key(**e)).is_none()).map(|(at, e)| (at, *e)).take(ROW_EVENTS + 1).collect();
            if gone.is_empty() && came.is_empty() || gone.len() + came.len() > ROW_EVENTS {
                reordered = true;
            } else {
                for &(at, e) in gone.iter().rev() {
                    self.event(&list, OBJECT_EVENT, "ChildrenChanged", ("remove", at as i32), Variant::from(self.row_ref(&old.rows, e)));
                }
                for &(at, e) in &came {
                    self.event(&list, OBJECT_EVENT, "ChildrenChanged", ("add", at as i32), Variant::from(self.row_ref(&new.rows, e)));
                }
            }
        }
        if reordered {
            self.event(&list, OBJECT_EVENT, "RowReordered", ("", 0), Variant::from(0i32));
        }

        // Selection, by key. Rows that left the list aren't told they're no longer selected.
        if same_folder {
            let (was, is) = (old.rows.selected_keys(), new.rows.selected_keys());
            let changed: Vec<&Cow<'_, [u8]>> = was.symmetric_difference(&is).filter(|k| new.rows.position_of(k).is_some()).collect();
            if changed.len() <= ROW_EVENTS {
                for k in &changed {
                    state(&row_path(new.rows.era, k), "selected", is.contains(*k));
                }
            }
            if !changed.is_empty() {
                self.event(&list, OBJECT_EVENT, "SelectionChanged", ("", 0), Variant::from(0i32));
            }
        } else {
            self.event(&list, OBJECT_EVENT, "SelectionChanged", ("", 0), Variant::from(0i32));
        }

        // Focus last, so a screen reader reads the row as it is now.
        let has = new.window_focused.then(|| path_of(new));
        if had != has {
            if let Some(p) = &had {
                let still_there = match old.focus {
                    a11y::Focus::Node(id) => new.get(id).is_some(),
                    a11y::Focus::Row(e) => same_folder && new.rows.position_of(&old.rows.key(e)).is_some(),
                };
                if still_there {
                    state(p, "focused", false);
                }
            }
            if let Some(p) = &has {
                if let a11y::Focus::Row(e) = new.focus {
                    self.event(&list, OBJECT_EVENT, "ActiveDescendantChanged", ("", new.rows.position(e).map_or(-1, |at| at as i32)), Variant::from(self.row_ref(&new.rows, e)));
                }
                state(p, "focused", true);
            }
        }
        self.emit_news(old, new);
        self.emit_dialog(old, new);
    }

    /// Announces what's new, and tells of jobs' progress.
    fn emit_news(&self, old: &Snapshot, new: &Snapshot) {
        for j in &new.news.jobs {
            if old.news.jobs.iter().any(|o| o.id == j.id && o.percent != j.percent) {
                self.event(&self.path(a11y::JOB0 + j.id), OBJECT_EVENT, "PropertyChange", ("accessible-value", 0), Variant::from(j.percent.map_or(0.0, f64::from)));
            }
        }
        let frame = self.path(a11y::FRAME);
        for (text, urgent) in new.news_since(old) {
            self.event(&frame, OBJECT_EVENT, "Announcement", ("", if urgent { ASSERTIVE } else { POLITE }), Variant::from(text));
        }
    }

    /// Announces a dialog as it opens, after the part it gives the keyboard, what it asks and
    /// what it says under that; and the passphrase prompt's line as it changes, which says
    /// when it's being tried and when it didn't work.
    fn emit_dialog(&self, old: &Snapshot, new: &Snapshot) {
        let Some(d) = new.get(a11y::DIALOG) else { return };
        let frame = self.path(a11y::FRAME);
        // The next clash can ask the same as the last, with the keyboard on the same button.
        let asked = |s: &Snapshot| s.modal.and_then(|m| m.asked);
        match old.get(a11y::DIALOG) {
            Some(o) if o.name == d.name && asked(old) == asked(new) => {
                if o.description != d.description && new.get(a11y::PASSPHRASE).is_some() {
                    self.event(&frame, OBJECT_EVENT, "Announcement", ("", ASSERTIVE), Variant::from(d.description.clone()));
                }
            }
            // Moving the keyboard into it reads it: a screen reader says the dialog the part
            // with the keyboard is in, its question and the line under it. Announcing it too
            // read everything twice. Only a question asked again with the keyboard where it
            // was, as the next clash can be, is announced, since nothing moves.
            _ => {
                if old.focus == new.focus
                    && let a11y::Focus::Node(id) = new.focus
                    && old.window_focused
                    && new.window_focused
                {
                    let said = match (d.name.as_str(), d.description.as_str()) {
                        (asks, "") => asks.to_string(),
                        (asks, says) if asks.ends_with(['?', '.', '!', '…']) => format!("{asks} {says}"),
                        (asks, says) => format!("{asks}. {says}"),
                    };
                    // Polite, so it follows what's said of the part with the keyboard.
                    self.event(&frame, OBJECT_EVENT, "Announcement", ("", POLITE), Variant::from(said));
                    self.event(&self.path(id), OBJECT_EVENT, "StateChanged", ("focused", 1), Variant::from(0i32));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_paths_name_the_folder_visit_and_the_file() {
        assert_eq!(row_path(3, b"a b\xff"), "/org/a11y/atspi/accessible/r3_612062ff");
        assert_eq!(parse_row("3_612062ff"), Some((3, b"a b\xff".to_vec())));
        assert_eq!(parse_row("3_6120620"), None, "half a byte");
        assert_eq!(parse_row("3_zz"), None);
        assert_eq!(parse_row("x_61"), None);
        assert_eq!(fixed_path(a11y::LIST), "/org/a11y/atspi/accessible/3");
        // A cell's path is its row's with its column first, and doesn't pass for a row's.
        assert_eq!(cell_path(3, 2, b"a b\xff"), "/org/a11y/atspi/accessible/c2_3_612062ff");
        assert_eq!(cell_path(3, 2, b"a").strip_prefix(PREFIX).and_then(|r| r.strip_prefix('r')), None);
        assert_eq!(bits(&[STATE_FOCUSED, STATE_MANAGES_DESCENDANTS, STATE_VISIBLE]), vec![(1 << 12) | (1 << 31) | (1 << 30), 0]);
        assert_eq!(bits(&[STATE_SELECTABLE_TEXT]), vec![0, 1 << 6]);
    }

    #[test]
    fn a_field_is_read_by_character_word_and_line() {
        let c: Vec<char> = "my report.pdf".chars().collect();
        assert_eq!(segment(&c, 4, Unit::Char), (4, 5));
        assert_eq!(segment(&c, 13, Unit::Char), (13, 13), "nothing past the end");
        assert_eq!(segment(&c, 0, Unit::Word), (0, 3), "a word and the space after it");
        assert_eq!(segment(&c, 2, Unit::Word), (0, 3));
        assert_eq!(segment(&c, 5, Unit::Word), (3, 10));
        assert_eq!(segment(&c, 11, Unit::Word), (10, 13));
        assert_eq!(segment(&c, 4, Unit::Line), (0, 13));
        assert_eq!(segment(&c, 13, Unit::Line), (0, 13));
        assert_eq!(before(&c, 5, Unit::Word), (0, 3));
        assert_eq!(before(&c, 1, Unit::Word), (0, 0), "nothing before the first word");
        assert_eq!(before(&c, 13, Unit::Word), (10, 13), "past the end, the last word is before");
        assert_eq!(before(&c, 4, Unit::Char), (3, 4));
        assert_eq!(after(&c, 1, Unit::Word), (3, 10));
        assert_eq!(after(&c, 11, Unit::Word), (13, 13));
        assert_eq!(after(&c, 12, Unit::Char), (13, 13));
        let c: Vec<char> = "..ab".chars().collect();
        assert_eq!(segment(&c, 0, Unit::Word), (0, 2), "what comes before the first word stands alone");
        assert_eq!(segment(&[], 0, Unit::Word), (0, 0));
        assert_eq!(before(&[], 0, Unit::Char), (0, 0));
    }

    #[test]
    fn text_changes_say_what_went_and_came_in_characters() {
        assert_eq!(text_change("report", "reports"), (6, String::new(), "s".into()));
        assert_eq!(text_change("aé日b", "ab"), (1, "é日".into(), String::new()));
        assert_eq!(text_change("report.pdf", "x.pdf"), (0, "report".into(), "x".into()));
        assert_eq!(text_change("aa", "aaa"), (2, String::new(), "a".into()));
        assert_eq!(text_change("same", "same"), (4, String::new(), String::new()));
    }
}
