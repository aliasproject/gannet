//! What a screen reader sees: a snapshot of the window's parts, built from the app on the UI
//! thread and served over AT-SPI by `a11y_atspi` on threads of its own.
//!
//! Nothing here runs unless a screen reader is listening: `Link::update` returns at once
//! otherwise. The file list's rows aren't copied into the snapshot one by one. It shares the
//! app's listing and row order, and the server reads each row from them when it's asked, so a
//! folder of a hundred thousand files takes no more memory to serve than one of ten, and a
//! question about one row costs the same in either. A change to the rows still costs time in
//! proportion to the folder, as it does for the app itself.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use gannet_core::listing::Listing;

use crate::app::App;
use crate::canvas::Rect;

/// Ids of the window's fixed parts. Places are by their place in the sidebar.
pub const FRAME: u64 = 1;
pub const SIDEBAR: u64 = 2;
pub const LIST: u64 = 3;
pub const BACK: u64 = 4;
pub const PLACE0: u64 = 100;
// The Open and Save dialog's bar. Its file types are by their place among the request's.
pub const CHOOSER_ACCEPT: u64 = 20;
pub const CHOOSER_CANCEL: u64 = 21;
pub const CHOOSER_SAVE_NAME: u64 = 22;
pub const CHOOSER_INFO: u64 = 23;
pub const CHOOSER_TYPES: u64 = 24;
pub const CHOOSER_TYPE0: u64 = 1 << 16;
// The preview window.
pub const PREVIEW: u64 = 50;
pub const PREVIEW_NAME: u64 = 51;
pub const PREVIEW_KIND: u64 = 52;
pub const PREVIEW_SHOWS: u64 = 53;
pub const PREVIEW_PREVIOUS: u64 = 54;
pub const PREVIEW_NEXT: u64 = 55;
pub const PREVIEW_CLOSE: u64 = 56;
// The Open With window. Its apps and headings are numbered from `OPEN_WITH_ROW0`, each keeping
// its number while the window is open.
pub const OPEN_WITH_SEARCH: u64 = 60;
pub const OPEN_WITH_APPS: u64 = 61;
pub const OPEN_WITH_ALWAYS: u64 = 62;
pub const OPEN_WITH_CANCEL: u64 = 63;
pub const OPEN_WITH_OPEN: u64 = 64;
pub const OPEN_WITH_ROW0: u64 = 1 << 20;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role {
    Frame,
    Dialog,
    List,
    ListItem,
    PushButton,
    CheckBox,
    /// A line of text that can be typed in. Its text is the node's `field`.
    Entry,
    Label,
    Heading,
    /// What the preview window shows.
    Document,
}

/// The one thing a part does when a screen reader asks: its name and description, as AT-SPI
/// has them, and the key that does it too.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Action {
    pub name: &'static str,
    pub description: &'static str,
    pub keys: &'static str,
}

impl Action {
    pub const fn new(name: &'static str, description: &'static str, keys: &'static str) -> Self {
        Self { name, description, keys }
    }
}

/// One of the window's fixed parts: everything but the file list's rows.
#[derive(Clone, PartialEq, Debug)]
pub struct Node {
    pub id: u64,
    pub role: Role,
    pub name: String,
    /// Said after the name: an app being the default, or what the preview shows.
    pub description: String,
    /// In the window's own pixels. Wayland never says where a window is on screen.
    pub rect: Rect,
    pub children: Vec<u64>,
    /// 0 for the frame, whose parent is the application.
    pub parent: u64,
    pub selected: bool,
    pub enabled: bool,
    /// Whether any of it is in view: a place can be scrolled out of the sidebar.
    pub showing: bool,
    pub action: Option<Action>,
    /// A check box's state.
    pub checked: Option<bool>,
    /// An entry's text, caret and selection.
    pub field: Option<crate::field::Field>,
}

impl Node {
    /// A part with nothing selected, checked or typed in, enabled and showing.
    pub fn new(id: u64, role: Role, name: impl Into<String>, rect: Rect, parent: u64) -> Self {
        Node { id, role, name: name.into(), description: String::new(), rect, children: Vec::new(), parent, selected: false, enabled: true, showing: true, action: None, checked: None, field: None }
    }

    pub fn described(mut self, d: impl Into<String>) -> Self {
        self.description = d.into();
        self
    }

    pub fn with_children(mut self, c: Vec<u64>) -> Self {
        self.children = c;
        self
    }

    pub fn doing(mut self, a: Action) -> Self {
        self.action = Some(a);
        self
    }

    pub fn enabled(mut self, on: bool) -> Self {
        self.enabled = on;
        self
    }

    pub fn selected(mut self, on: bool) -> Self {
        self.selected = on;
        self
    }

    pub fn showing(mut self, on: bool) -> Self {
        self.showing = on;
        self
    }
}

/// A push button's action.
pub const fn click(description: &'static str, keys: &'static str) -> Action {
    Action::new("click", description, keys)
}

/// The folder shown, and what's needed to say where a row of Recent, the Trash or a search is.
#[derive(Default, Debug)]
pub struct Here {
    pub dir: PathBuf,
    /// What the folder's called, as the window's title says it.
    pub title: String,
    pub home: Option<PathBuf>,
}

/// The file list's rows, read from the app's own listing.
#[derive(Clone, Default)]
pub struct Rows {
    /// Changes whenever the window shows another folder. A row is known by this and its key
    /// (`Listing::key`: the file's name, or its path in the Trash, Recent and search results,
    /// where two rows can share a name), which stay the same however often the folder is read again, so a
    /// screen reader isn't told of a new row every time a file in the folder changes. The key
    /// is part of what it's known by, so a screen reader that kept it can't be handed another
    /// file's.
    pub era: u64,
    pub listing: Option<Arc<Listing>>,
    /// Listing entries, in the order shown.
    pub order: Arc<Vec<u32>>,
    /// Where each key is in `order`.
    pub index: Arc<HashMap<Box<[u8]>, u32>>,
    /// One bit for each listing entry, as the app keeps them.
    pub selected: Arc<Vec<u64>>,
    /// The list's area, the height of a row and how far it's scrolled.
    pub area: Rect,
    pub row_h: i32,
    pub scroll: f64,
    /// Changes only with `era`.
    pub here: Arc<Here>,
}

impl std::fmt::Debug for Rows {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Rows {{ era: {}, rows: {}, scroll: {} }}", self.era, self.order.len(), self.scroll)
    }
}

impl PartialEq for Rows {
    fn eq(&self, o: &Self) -> bool {
        let same_listing = match (&self.listing, &o.listing) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        };
        self.era == o.era && same_listing && Arc::ptr_eq(&self.order, &o.order) && Arc::ptr_eq(&self.selected, &o.selected) && self.area == o.area && self.row_h == o.row_h && self.scroll == o.scroll
    }
}

impl Rows {
    pub fn len(&self) -> usize {
        self.order.len()
    }

    /// The listing entry at a place in the list.
    pub fn entry(&self, at: usize) -> Option<u32> {
        self.order.get(at).copied()
    }

    /// The file name of a listing entry, as it is on disk.
    pub fn raw_name(&self, entry: u32) -> &[u8] {
        self.listing.as_deref().filter(|l| (entry as usize) < l.len()).map_or(&[], |l| l.name(entry))
    }

    pub fn name(&self, entry: u32) -> String {
        String::from_utf8_lossy(self.raw_name(entry)).into_owned()
    }

    /// Where a row is, as the list's Location column says it, in the views whose rows come
    /// from many folders: Recent, the Trash and search results.
    pub fn location(&self, entry: u32) -> Option<String> {
        let l = self.listing.as_deref().filter(|l| (entry as usize) < l.len())?;
        let folder = l.location(entry)?;
        Some(if l.search().is_some() { crate::app::search_location(folder, &self.here.dir, &self.here.title) } else { crate::app::home_location(folder, self.here.home.as_deref()) })
    }

    /// What tells a listing entry's row apart from the others: `Listing::key`.
    pub fn key(&self, entry: u32) -> Cow<'_, [u8]> {
        self.listing.as_deref().filter(|l| (entry as usize) < l.len()).map_or(Cow::Borrowed(&[]), |l| l.key(entry))
    }

    /// Where the row of that key is in the list, if it's shown.
    pub fn position_of(&self, key: &[u8]) -> Option<usize> {
        self.index.get(key).map(|&at| at as usize)
    }

    /// Where a listing entry is in the list, if it's shown.
    pub fn position(&self, entry: u32) -> Option<usize> {
        self.position_of(&self.key(entry)).filter(|&at| self.entry(at) == Some(entry))
    }

    pub fn is_selected(&self, entry: u32) -> bool {
        self.selected.get(entry as usize / 64).is_some_and(|w| w >> (entry % 64) & 1 == 1)
    }

    /// The keys of the selected rows.
    pub fn selected_keys(&self) -> HashSet<Cow<'_, [u8]>> {
        let mut out = HashSet::new();
        for (w, &word) in self.selected.iter().enumerate() {
            let mut bits = word;
            while bits != 0 {
                let e = w as u32 * 64 + bits.trailing_zeros();
                bits &= bits - 1;
                if self.position(e).is_some() {
                    out.insert(self.key(e));
                }
            }
        }
        out
    }

    /// The row's area, in the window. A row scrolled out of view is outside the list's area.
    pub fn rect(&self, at: usize) -> Rect {
        let y = self.area.y + (at as f64 * self.row_h as f64 - self.scroll).round() as i32;
        Rect::new(self.area.x, y, self.area.w, self.row_h)
    }

    /// Whether any of the row is in view.
    pub fn showing(&self, at: usize) -> bool {
        let r = self.rect(at);
        r.y + r.h > self.area.y && r.y < self.area.y + self.area.h
    }
}

/// What has the keyboard.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Focus {
    Node(u64),
    /// A listing entry.
    Row(u32),
}

#[derive(Clone, PartialEq, Debug)]
pub struct Snapshot {
    /// The frame first.
    pub fixed: Vec<Node>,
    pub rows: Rows,
    pub focus: Focus,
    pub window_focused: bool,
    /// The folder shown.
    pub dir: Arc<PathBuf>,
}

impl Snapshot {
    pub fn get(&self, id: u64) -> Option<&Node> {
        self.fixed.iter().find(|n| n.id == id)
    }
}

/// Something a screen reader asked for.
pub enum Act {
    /// Do a part's action: press a button, tick a box, or open with an app.
    Press(u64),
    /// Give a part the keyboard: an entry, or an item of a list other than the file list, which
    /// picks it, as moving to it does with the keyboard.
    Focus(u64),
    /// Open a place in the sidebar.
    OpenPlace(usize),
    /// Scroll the sidebar to show a place.
    ShowPlace(usize),
    /// Put the cursor on a row without changing the selection: the row of that key
    /// (`Listing::key`) in that folder. A request about a folder the window has left is dropped,
    /// here and for each request about a row.
    FocusRow(Arc<PathBuf>, Vec<u8>),
    /// Add a row to the selection (true) or take it out, leaving the rest as it is.
    SelectRow(Arc<PathBuf>, Vec<u8>, bool),
    /// Scroll a row into view without moving the cursor.
    ShowRow(Arc<PathBuf>, Vec<u8>),
    /// Someone started listening: hand over a snapshot.
    Wake,
}

/// What the server is told.
pub enum ToServer {
    /// The window as it is now.
    Snapshot(Snapshot),
    /// The registry restarted: introduce the app again.
    Embed,
    /// Stop serving: the screen reader has gone.
    Stop,
    /// The connection to the accessibility bus closed.
    Lost,
}

/// Shared by the UI thread's `Link`, the session watcher and the server, while there is one.
#[derive(Default)]
pub struct Shared {
    live: AtomicBool,
    /// Which server is current. Each start bumps it, so the next snapshot goes to the new one
    /// whole, and an old one stopping late can't disconnect its successor.
    epoch: AtomicU64,
    to_server: Mutex<Option<(u64, Sender<ToServer>)>>,
}

impl Shared {
    /// A new server's number, for `connect` and `stopped`.
    pub fn next_epoch(&self) -> u64 {
        self.epoch.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Server `epoch` is serving: snapshots go to `tx` from now on. False if it has been
    /// stopped, or a newer one started, while it was getting ready: then it should stop.
    pub fn connect(&self, epoch: u64, tx: Sender<ToServer>) -> bool {
        let mut to = self.to_server.lock().unwrap();
        if self.epoch.load(Ordering::SeqCst) != epoch {
            return false;
        }
        *to = Some((epoch, tx));
        self.live.store(true, Ordering::SeqCst);
        true
    }

    /// Stops the current server, and any still getting ready.
    pub fn disconnect(&self) {
        let _to = self.to_server.lock().unwrap();
        self.epoch.fetch_add(1, Ordering::SeqCst);
        drop(_to);
        self.live.store(false, Ordering::SeqCst);
        if let Some((_, tx)) = self.to_server.lock().unwrap().take() {
            let _ = tx.send(ToServer::Stop);
        }
    }

    /// Server `epoch` has stopped on its own, as when its bus went away.
    pub fn stopped(&self, epoch: u64) {
        let mut to = self.to_server.lock().unwrap();
        if to.as_ref().is_some_and(|(e, _)| *e == epoch) {
            self.live.store(false, Ordering::SeqCst);
            to.take();
        }
    }

    pub fn is_live(&self) -> bool {
        self.live.load(Ordering::SeqCst)
    }

    fn current_epoch(&self) -> u64 {
        self.epoch.load(Ordering::SeqCst)
    }
}

/// The UI thread's side: builds a snapshot after each change, while someone is listening.
pub struct Link {
    pub shared: Arc<Shared>,
    last: Option<Snapshot>,
    epoch: u64,
    dir: Arc<PathBuf>,
    era: u64,
    here: Arc<Here>,
    no_rows: Rows,
}

impl Link {
    pub fn new() -> Self {
        Self { shared: Arc::default(), last: None, epoch: 0, dir: Arc::default(), era: 0, here: Arc::default(), no_rows: Rows::default() }
    }

    /// Hands the server a snapshot of the app's window if anything changed. Costs nothing
    /// while no one listens.
    pub fn update(&mut self, app: &App) {
        if self.listening() {
            let snap = self.build(app);
            self.offer(snap);
        }
    }

    /// Whether a screen reader is listening, so a snapshot is wanted. Forgets the last one
    /// when not, and when a new server started, which is sent the next one whole.
    pub fn listening(&mut self) -> bool {
        if !self.shared.is_live() {
            self.last = None;
            return false;
        }
        let epoch = self.shared.current_epoch();
        if epoch != self.epoch {
            self.epoch = epoch;
            self.last = None;
        }
        true
    }

    /// Hands the server a snapshot built elsewhere, as the Open With window's is, if it isn't
    /// the same as the last. Ask `listening` first.
    pub fn offer(&mut self, snap: Snapshot) {
        if self.last.as_ref() == Some(&snap) {
            return;
        }
        if let Some((_, tx)) = self.shared.to_server.lock().unwrap().as_ref() {
            let _ = tx.send(ToServer::Snapshot(snap.clone()));
        }
        self.last = Some(snap);
    }

    /// No rows, the same each time, so a window without a file list compares as unchanged.
    pub fn no_rows(&self) -> Rows {
        self.no_rows.clone()
    }

    fn build(&mut self, app: &App) -> Snapshot {
        if app.viewer {
            return self.build_viewer(app);
        }
        if *self.dir != app.dir {
            self.dir = Arc::new(app.dir.clone());
            self.era += 1;
            // The same home as the Location column uses.
            self.here = Arc::new(Here { dir: app.dir.clone(), title: app.crumb_label(&app.dir), home: std::env::var_os("HOME").map(PathBuf::from) });
        }
        let prev = self.last.as_ref().map(|s| &s.rows).filter(|r| r.era == self.era);
        let same_listing = |r: &Rows| match (&r.listing, &app.listing) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        };
        // The row order and selection are shared with the last snapshot while they're the same,
        // which is what makes comparing snapshots cheap.
        let (order, index) = match prev {
            Some(r) if same_listing(r) && r.order.as_slice() == app.rows.as_slice() => (r.order.clone(), r.index.clone()),
            _ => {
                let names = app.listing.as_deref();
                let index: HashMap<Box<[u8]>, u32> = app.rows.iter().enumerate().filter_map(|(at, &e)| Some((Box::from(&*names.filter(|l| (e as usize) < l.len())?.key(e)), at as u32))).collect();
                (Arc::new(app.rows.clone()), Arc::new(index))
            }
        };
        let selected = match prev {
            Some(r) if same_listing(r) && r.selected.as_slice() == app.selected_bits() => r.selected.clone(),
            _ => Arc::new(app.selected_bits().to_vec()),
        };
        let l = app.layout();
        let rows = Rows { era: self.era, listing: app.listing.clone(), order, index, selected, area: l.list, row_h: l.row_h.max(1), scroll: app.scroll, here: self.here.clone() };

        let place_h = app.px(30.0);
        let places: Vec<Node> = app.places.iter().enumerate().map(|(i, p)| {
                // Placed as drawn, with the sidebar scrolled.
                let rect = Rect::new(l.sidebar.x, app.place_top(i), l.sidebar.w, place_h);
                let showing = rect.y + rect.h > l.sidebar.y && rect.y < l.sidebar.y + l.sidebar.h;
                Node::new(PLACE0 + i as u64, Role::ListItem, p.label.clone(), rect, SIDEBAR).selected(p.path == app.dir).showing(showing).doing(Action::new("activate", "Open", ""))
            })
            .collect();
        let b = app.px(36.0);
        // As the window's title says it, so Recent and the Trash are called that.
        let title = app.crumb_label(&app.dir);
        let mut fixed = vec![
            Node::new(FRAME, Role::Frame, format!("{title} - Gannet"), Rect::new(0, 0, app.w, app.h), 0).with_children(vec![BACK, SIDEBAR, LIST]),
            Node::new(BACK, Role::PushButton, "Back", Rect::new(app.px(6.0), 0, b, l.toolbar.h), FRAME).enabled(app.history.can_go_back()).doing(click("Go back", "<Alt>Left")),
            Node::new(SIDEBAR, Role::List, "Places", l.sidebar, FRAME).with_children(places.iter().map(|n| n.id).collect()),
            Node::new(LIST, Role::List, "Files", l.list, FRAME),
        ];
        fixed.extend(places);
        let mut focus = match rows.entry(app.cursor) {
            Some(e) if app.cursor_shown => Focus::Row(e),
            _ => Focus::Node(LIST),
        };
        if let Some(c) = app.chooser.as_ref() {
            // A dialog for another program, named for what it asked, and where it is.
            fixed[0].role = Role::Dialog;
            fixed[0].name = format!("{} - {title}", c.spec.title());
            let parts = app.a11y_chooser_parts();
            fixed[0].children.extend(parts.iter().filter(|n| n.parent == FRAME).map(|n| n.id));
            if parts.iter().any(|n| n.id == CHOOSER_SAVE_NAME && n.field.is_some()) && app.a11y_name_has_keyboard() {
                focus = Focus::Node(CHOOSER_SAVE_NAME);
            }
            fixed.extend(parts);
        }
        Snapshot { fixed, rows, focus, window_focused: app.has_keyboard, dir: self.dir.clone() }
    }

    /// The preview window: the file's name, its kind and what's shown of it, and buttons to
    /// walk its folder and close it.
    fn build_viewer(&mut self, app: &App) -> Snapshot {
        let s = app.a11y_preview();
        let r = Rect::new(0, 0, app.w, app.h);
        let fixed = vec![
            Node::new(FRAME, Role::Frame, if s.name.is_empty() { "Preview".to_string() } else { format!("{} - Preview", s.name) }, r, 0).with_children(vec![PREVIEW, PREVIEW_PREVIOUS, PREVIEW_NEXT, PREVIEW_CLOSE]),
            // What has the keyboard, so moving to another file reads out its name, and the rest
            // is there to be read.
            Node::new(PREVIEW, Role::Document, s.name.clone(), s.area, FRAME).described([s.kind.as_str(), s.shows.as_str()].into_iter().filter(|t| !t.is_empty()).collect::<Vec<_>>().join(", ")).with_children(vec![PREVIEW_NAME, PREVIEW_KIND, PREVIEW_SHOWS]),
            Node::new(PREVIEW_NAME, Role::Label, s.name, s.title, PREVIEW),
            Node::new(PREVIEW_KIND, Role::Label, s.kind, s.area, PREVIEW),
            Node::new(PREVIEW_SHOWS, Role::Label, s.shows, s.foot, PREVIEW),
            // Placed as the keys that do the same: there are no buttons on screen.
            Node::new(PREVIEW_PREVIOUS, Role::PushButton, "Previous file", Rect::default(), FRAME).enabled(s.can_step.0).doing(click("Show the previous file in the folder", "Left")),
            Node::new(PREVIEW_NEXT, Role::PushButton, "Next file", Rect::default(), FRAME).enabled(s.can_step.1).doing(click("Show the next file in the folder", "Right")),
            Node::new(PREVIEW_CLOSE, Role::PushButton, "Close", Rect::default(), FRAME).doing(click("Close the preview", "Escape")),
        ];
        Snapshot { fixed, rows: self.no_rows(), focus: Focus::Node(PREVIEW), window_focused: app.has_keyboard, dir: self.dir.clone() }
    }
}

/// What the preview window says about the file it shows.
pub struct PreviewText {
    pub name: String,
    /// Its kind, like "PNG image".
    pub kind: String,
    /// What's shown of it: a picture's size, a document's lines, or why it can't be shown.
    pub shows: String,
    pub title: Rect,
    pub area: Rect,
    pub foot: Rect,
    /// Whether there's a file before it and after it.
    pub can_step: (bool, bool),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(n: usize, scroll: f64) -> Rows {
        let order: Vec<u32> = (0..n as u32).rev().collect();
        Rows { era: 1, listing: None, order: Arc::new(order), index: Arc::default(), selected: Arc::new(vec![0b100]), area: Rect::new(10, 100, 300, 200), row_h: 20, scroll, here: Arc::default() }
    }

    #[test]
    fn rows_are_placed_by_scroll() {
        let r = rows(50, 30.0);
        assert_eq!(r.entry(0), Some(49));
        assert!(r.is_selected(2) && !r.is_selected(3));
        assert_eq!(r.rect(2), Rect::new(10, 110, 300, 20));
        assert!(!r.showing(0), "scrolled out of view above");
        assert!(r.showing(1), "partly in view");
        assert!(r.showing(11) && !r.showing(12), "the list is 200 high");
    }

    #[test]
    fn rows_compare_by_what_they_share() {
        let a = rows(5, 0.0);
        let mut b = a.clone();
        assert_eq!(a, b);
        b.order = Arc::new(a.order.to_vec());
        assert_ne!(a, b, "a new order is a change, even with the same contents: the link shares it while nothing changes");
        let mut c = a.clone();
        c.era += 1;
        assert_ne!(a, c);
    }
}
