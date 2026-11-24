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

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex};

use gannet_core::bus::{Bus, zbus};
use zbus::message::{Body, Header, Type};
use zbus::{MatchRule, Message, OwnedObjectPath, Variant};

use crate::a11y::{self, Act, Node, Role, Rows, Shared, Snapshot, ToServer};
use crate::app::{Hub, Msg};

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
const STATE_ENABLED: u32 = 8;
const STATE_FOCUSABLE: u32 = 11;
const STATE_FOCUSED: u32 = 12;
const STATE_MULTISELECTABLE: u32 = 18;
const STATE_SELECTABLE: u32 = 22;
const STATE_SELECTED: u32 = 23;
const STATE_SENSITIVE: u32 = 24;
const STATE_SHOWING: u32 = 25;
const STATE_SINGLE_LINE: u32 = 26;
const STATE_VISIBLE: u32 = 30;
const STATE_MANAGES_DESCENDANTS: u32 = 31;
const STATE_CHECKABLE: u32 = 41;
const STATE_READ_ONLY: u32 = 43;

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
    let ctx = Arc::new(Ctx { bus: bus.clone(), me, now: Mutex::default(), hub: hub.clone(), desktop: Mutex::new(null()), id: Mutex::new(0) });
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
            Ok(ToServer::Snapshot(s)) => break Arc::new(s),
            // Not introduced yet, so there's nothing to introduce again.
            Ok(ToServer::Embed) => {}
            Ok(ToServer::Stop) | Err(_) => return Ok(()),
            Ok(ToServer::Lost) => return Err(zbus::Error::Failure("lost the accessibility bus".into())),
        }
    };
    *ctx.now.lock().unwrap() = Some(old.clone());
    ctx.embed()?;
    for m in rx {
        match m {
            ToServer::Snapshot(s) => {
                let new = Arc::new(s);
                *ctx.now.lock().unwrap() = Some(new.clone());
                ctx.emit_changes(&old, &new);
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
    }
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
}

impl Ctx {
    fn embed(&self) -> zbus::Result<()> {
        let (desktop,): (Ref,) = self.bus.call(REGISTRY, ROOT, "org.a11y.atspi.Socket", "Embed", &((self.me.clone(), OwnedObjectPath::try_from(ROOT)?),))?;
        *self.desktop.lock().unwrap() = desktop;
        Ok(())
    }

    fn at(&self, path: &str) -> Ref {
        (self.me.clone(), OwnedObjectPath::try_from(path).unwrap_or_else(|_| OwnedObjectPath::try_from(NULL).unwrap()))
    }

    fn fixed_ref(&self, id: u64) -> Ref {
        self.at(&fixed_path(id))
    }

    fn row_ref(&self, rows: &Rows, entry: u32) -> Ref {
        self.at(&row_path(rows.era, &rows.key(entry)))
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
        self.signal(path, iface, member, &(kind, d1, 0i32, data, HashMap::<&str, Variant<'_>>::new()));
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
            match rest.strip_prefix('r') {
                Some(row) => {
                    let found = parse_row(row).filter(|(era, _)| *era == snap.rows.era).and_then(|(_, key)| snap.rows.position_of(&key)).and_then(|at| Some((snap.rows.entry(at)?, at)));
                    match found {
                        Some((e, at)) => Target::Row(e, at),
                        None => return self.error(&h, UNKNOWN_OBJECT, "gone"),
                    }
                }
                None => match rest.parse().ok().and_then(|id| snap.get(id)) {
                    Some(n) => Target::Fixed(n),
                    None => return self.error(&h, UNKNOWN_OBJECT, "gone"),
                },
            }
        } else {
            return self.error(&h, UNKNOWN_OBJECT, "no such object");
        };
        let body = m.body();
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
            Target::Row(..) => 0,
        }
    }

    fn child(&self, target: &Target<'_>, snap: &Snapshot, i: usize) -> Option<Ref> {
        match target {
            Target::Root => (i == 0).then(|| self.fixed_ref(a11y::FRAME)),
            Target::Fixed(n) if n.id == a11y::LIST => snap.rows.entry(i).map(|e| self.row_ref(&snap.rows, e)),
            Target::Fixed(n) => n.children.get(i).map(|&c| self.fixed_ref(c)),
            Target::Row(..) => None,
        }
    }

    fn parent(&self, target: &Target<'_>) -> Ref {
        match target {
            Target::Root => self.desktop.lock().unwrap().clone(),
            Target::Fixed(n) if n.parent == 0 => self.at(ROOT),
            Target::Fixed(n) => self.fixed_ref(n.parent),
            Target::Row(..) => self.fixed_ref(a11y::LIST),
        }
    }

    fn index_in_parent(&self, target: &Target<'_>, snap: &Snapshot) -> i32 {
        match target {
            Target::Root => -1,
            Target::Fixed(n) if n.parent == 0 => 0,
            Target::Fixed(n) => snap.get(n.parent).and_then(|p| p.children.iter().position(|&c| c == n.id)).map_or(-1, |i| i as i32),
            Target::Row(_, at) => *at as i32,
        }
    }

    fn name(&self, target: &Target<'_>, snap: &Snapshot) -> String {
        match target {
            Target::Root => "Gannet".into(),
            Target::Fixed(n) => n.name.clone(),
            Target::Row(e, _) => snap.rows.name(*e),
        }
    }

    /// Where a row of Recent, the Trash or a search is, so two of one name can be told apart,
    /// or what a part says besides its name.
    fn description(&self, target: &Target<'_>, snap: &Snapshot) -> String {
        match target {
            Target::Row(e, _) => snap.rows.location(*e).unwrap_or_default(),
            Target::Fixed(n) => n.description.clone(),
            Target::Root => String::new(),
        }
    }

    fn role(&self, target: &Target<'_>) -> (u32, &'static str) {
        match target {
            Target::Root => (ROLE_APPLICATION, "application"),
            Target::Fixed(n) => role_of(n.role),
            Target::Row(..) => role_of(Role::ListItem),
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
                if n.enabled {
                    on.extend([STATE_ENABLED, STATE_SENSITIVE]);
                }
                match n.role {
                    Role::Frame | Role::Dialog if snap.window_focused => on.push(STATE_ACTIVE),
                    Role::ListItem => on.extend([STATE_SELECTABLE, STATE_FOCUSABLE]),
                    Role::PushButton => on.push(STATE_FOCUSABLE),
                    Role::CheckBox => on.extend([STATE_FOCUSABLE, STATE_CHECKABLE]),
                    Role::Entry => on.extend([STATE_FOCUSABLE, STATE_EDITABLE, STATE_SINGLE_LINE]),
                    Role::Document => on.extend([STATE_FOCUSABLE, STATE_READ_ONLY]),
                    Role::List if n.id == a11y::LIST => on.extend([STATE_FOCUSABLE, STATE_MULTISELECTABLE, STATE_MANAGES_DESCENDANTS]),
                    Role::List => on.push(STATE_FOCUSABLE),
                    _ => {}
                }
                if n.selected {
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
                let mut on = vec![STATE_VISIBLE, STATE_ENABLED, STATE_SENSITIVE, STATE_SELECTABLE, STATE_FOCUSABLE];
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
        }
    }

    fn rect(&self, target: &Target<'_>, snap: &Snapshot) -> Option<crate::canvas::Rect> {
        match target {
            Target::Root => None,
            Target::Fixed(n) => Some(n.rect),
            Target::Row(_, at) => Some(snap.rows.rect(*at)),
        }
    }

    fn is_place(n: &Node) -> bool {
        n.parent == a11y::SIDEBAR
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
                ("Parent", Variant::from(self.parent(target))),
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
            (SELECTION, Target::Fixed(n)) if n.role == Role::List => vec![("NSelectedChildren", Variant::from(self.selected_children(n, snap).len() as i32))],
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
            "GetAttributes" => self.reply(h, &(HashMap::from([("toolkit", "Gannet")]),)),
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
                if matches!(target, Target::Fixed(n) if n.role == Role::List) {
                    v.push(SELECTION);
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
                    // A place scrolled out of the sidebar isn't there to point at.
                    Target::Fixed(n) => n.children.iter().filter_map(|&c| snap.get(c)).find(|c| r.contains(x, y) && c.rect.contains(x, y)).map(|c| self.fixed_ref(c.id)),
                    _ => None,
                };
                self.reply(h, &(hit.unwrap_or_else(null),))
            }
            "GrabFocus" => {
                let ok = match target {
                    Target::Row(e, _) => self.tell_row(snap, *e, Act::FocusRow),
                    // The keyboard doesn't go to the sidebar, but a place a screen reader moves
                    // to is shown.
                    Target::Fixed(n) if Self::is_place(n) => self.show_place(n, snap),
                    // An entry, or an item of another list, which moving to picks.
                    Target::Fixed(n) if matches!(n.role, Role::Entry | Role::ListItem) && n.enabled => {
                        self.hub.send(Msg::A11y(Act::Focus(n.id)));
                        true
                    }
                    _ => false,
                };
                self.reply(h, &(ok,))
            }
            "ScrollTo" | "ScrollToPoint" => {
                let ok = match target {
                    Target::Row(e, _) => self.tell_row(snap, *e, Act::ShowRow),
                    Target::Fixed(n) if Self::is_place(n) => self.show_place(n, snap),
                    _ => false,
                };
                self.reply(h, &(ok,))
            }
            "SetExtents" | "SetPosition" | "SetSize" => self.reply(h, &(false,)),
            _ => self.error(h, UNKNOWN_METHOD, m),
        }
    }

    /// A place's index among the sidebar's places.
    fn place_index(n: &Node, snap: &Snapshot) -> Option<usize> {
        snap.get(a11y::SIDEBAR).and_then(|s| s.children.iter().position(|&c| c == n.id))
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
                let ok = i == 0 && n.enabled;
                if ok {
                    let act = if Self::is_place(n) { Act::OpenPlace(Self::place_index(n, snap).unwrap_or(usize::MAX)) } else { Act::Press(n.id) };
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

    fn selection(&self, h: &Header<'_>, m: &str, body: &Body, target: &Target<'_>, snap: &Snapshot) {
        let Target::Fixed(n) = target else { return self.error(h, UNKNOWN_METHOD, m) };
        if n.role != Role::List {
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
                    Some(at) if n.id == a11y::SIDEBAR && at < n.children.len() => {
                        self.hub.send(Msg::A11y(Act::OpenPlace(at)));
                        true
                    }
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

    /// Tells listeners what changed between two snapshots.
    fn emit_changes(&self, old: &Snapshot, new: &Snapshot) {
        let state = |path: &str, what: &str, on: bool| self.event(path, OBJECT_EVENT, "StateChanged", (what, on as i32), Variant::from(0i32));
        let frame = fixed_path(a11y::FRAME);
        if old.window_focused != new.window_focused {
            let member = if new.window_focused { "Activate" } else { "Deactivate" };
            self.event(&frame, WINDOW_EVENT, member, ("", 0), Variant::from(""));
            state(&frame, "active", new.window_focused);
        }

        // The fixed parts: names, whether they can be used, and the sidebar's places.
        for n in &new.fixed {
            let Some(o) = old.get(n.id) else { continue };
            let path = fixed_path(n.id);
            if o.name != n.name {
                self.event(&path, OBJECT_EVENT, "PropertyChange", ("accessible-name", 0), Variant::from(n.name.clone()));
            }
            if o.description != n.description {
                self.event(&path, OBJECT_EVENT, "PropertyChange", ("accessible-description", 0), Variant::from(n.description.clone()));
            }
            if o.checked != n.checked {
                state(&path, "checked", n.checked == Some(true));
            }
            if o.enabled != n.enabled {
                state(&path, "enabled", n.enabled);
                state(&path, "sensitive", n.enabled);
            }
            // A place scrolling into or out of the sidebar.
            if o.showing != n.showing {
                state(&path, "showing", n.showing);
            }
            // A place is "selected" while the window is in it, which only marks it: announcing
            // that would read out the sidebar each time the folder changes, while the keyboard
            // is in the list. A screen reader asking still hears it.
            if o.selected != n.selected && n.parent != a11y::SIDEBAR {
                state(&path, "selected", n.selected);
                if n.role == Role::ListItem {
                    self.event(&fixed_path(n.parent), OBJECT_EVENT, "SelectionChanged", ("", 0), Variant::from(0i32));
                }
            }
        }
        // Parts coming and going: places, and the Open With window's apps as a search narrows
        // them.
        for n in &new.fixed {
            let Some(o) = old.get(n.id).filter(|o| o.children != n.children) else { continue };
            let path = fixed_path(n.id);
            for (i, id) in o.children.iter().enumerate().rev().filter(|(_, id)| !n.children.contains(id)) {
                self.event(&path, OBJECT_EVENT, "ChildrenChanged", ("remove", i as i32), Variant::from(self.fixed_ref(*id)));
            }
            for (i, id) in n.children.iter().enumerate().filter(|(_, id)| !o.children.contains(id)) {
                self.event(&path, OBJECT_EVENT, "ChildrenChanged", ("add", i as i32), Variant::from(self.fixed_ref(*id)));
            }
        }

        // The rows. Within one folder, a few rows coming or going are announced one by one.
        let list = fixed_path(a11y::LIST);
        let same_folder = old.rows.era == new.rows.era;
        if same_folder && !Arc::ptr_eq(&old.rows.order, &new.rows.order) {
            let gone: Vec<(usize, u32)> = old.rows.order.iter().enumerate().filter(|(_, e)| new.rows.position_of(&old.rows.key(**e)).is_none()).map(|(at, e)| (at, *e)).take(ROW_EVENTS + 1).collect();
            let came: Vec<(usize, u32)> = new.rows.order.iter().enumerate().filter(|(_, e)| old.rows.position_of(&new.rows.key(**e)).is_none()).map(|(at, e)| (at, *e)).take(ROW_EVENTS + 1).collect();
            if gone.len() + came.len() <= ROW_EVENTS {
                for &(at, e) in gone.iter().rev() {
                    self.event(&list, OBJECT_EVENT, "ChildrenChanged", ("remove", at as i32), Variant::from(self.row_ref(&old.rows, e)));
                }
                for &(at, e) in &came {
                    self.event(&list, OBJECT_EVENT, "ChildrenChanged", ("add", at as i32), Variant::from(self.row_ref(&new.rows, e)));
                }
            }
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
        let path_of = |s: &Snapshot| match s.focus {
            a11y::Focus::Node(id) => fixed_path(id),
            a11y::Focus::Row(e) => row_path(s.rows.era, &s.rows.key(e)),
        };
        let had = old.window_focused.then(|| path_of(old));
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
        assert_eq!(bits(&[STATE_FOCUSED, STATE_MANAGES_DESCENDANTS, STATE_VISIBLE]), vec![(1 << 12) | (1 << 31) | (1 << 30), 0]);
    }
}
