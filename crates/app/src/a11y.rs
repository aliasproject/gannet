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

use crate::app::{App, Edit};
use crate::canvas::Rect;
use crate::field::Field;

/// Ids of the window's fixed parts. Places are by their place in the sidebar.
pub const FRAME: u64 = 1;
pub const SIDEBAR: u64 = 2;
pub const LIST: u64 = 3;
pub const BACK: u64 = 4;
/// The typed path, while it's open.
pub const PATH_FIELD: u64 = 5;
/// The box that filters the list or searches below, while it's showing.
pub const FILTER: u64 = 6;
/// The field a row is renamed in, or a new folder named in.
pub const NAME_FIELD: u64 = 7;
pub const PLACE0: u64 = 100;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role {
    Frame,
    List,
    ListItem,
    PushButton,
    /// A single line of editable text, served with its `Node::field`.
    Entry,
}

/// One of the window's fixed parts: everything but the file list's rows.
#[derive(Clone, PartialEq, Debug)]
pub struct Node {
    pub id: u64,
    pub role: Role,
    pub name: String,
    /// In the window's own pixels. Wayland never says where a window is on screen.
    pub rect: Rect,
    pub children: Vec<u64>,
    /// 0 for the frame, whose parent is the application.
    pub parent: u64,
    pub selected: bool,
    pub enabled: bool,
    /// Whether any of it is in view: a place can be scrolled out of the sidebar.
    pub showing: bool,
    /// What it's for, beyond its name: the filter box says how far it reaches.
    pub description: String,
    /// An entry's text, caret and selection.
    pub field: Option<Field>,
}

/// Where a field's characters are drawn, for a screen reader that shows or magnifies them.
#[derive(Clone, PartialEq, Debug)]
pub struct TextGeometry {
    /// The text, and how far from its start each character's left edge is, and the last one's
    /// right edge. Measured once for each text: the field scrolling with its caret only moves it.
    measured: Arc<(String, Vec<f32>)>,
    /// The left edge of each character, and the right edge of the last.
    pub edges: Vec<i32>,
    /// The line the text is on.
    pub line: Rect,
}

impl TextGeometry {
    /// A character's box, or an empty box at the end, past the last one.
    pub fn char_rect(&self, at: usize) -> Rect {
        let last = self.edges.len().saturating_sub(1);
        let (a, b) = (self.edges.get(at.min(last)).copied().unwrap_or(0), self.edges.get((at + 1).min(last)).copied().unwrap_or(0));
        Rect::new(a, self.line.y, b - a, self.line.h)
    }

    /// The character at a point on the line, if there's one there.
    pub fn char_at(&self, x: i32, y: i32) -> Option<usize> {
        if y < self.line.y || y >= self.line.y + self.line.h {
            return None;
        }
        self.edges.windows(2).position(|e| x >= e[0] && x < e[1])
    }
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
    /// Where each field's characters are, by the field's id.
    pub text_geometry: Vec<(u64, Arc<TextGeometry>)>,
    /// Which field is open, as `App::edit_gen` counts them: a request about one since closed
    /// is dropped.
    pub edit_gen: u64,
}

impl Snapshot {
    pub fn get(&self, id: u64) -> Option<&Node> {
        self.fixed.iter().find(|n| n.id == id)
    }

    pub fn geometry(&self, id: u64) -> Option<&TextGeometry> {
        self.text_geometry.iter().find(|(i, _)| *i == id).map(|(_, g)| &**g)
    }
}

/// A change a screen reader asks of a field. Offsets are in characters, as AT-SPI counts.
#[derive(Clone, PartialEq, Debug)]
pub enum TextEdit {
    /// Move the caret, leaving nothing selected.
    Caret(usize),
    /// Select from the first to the second.
    Select(usize, usize),
    /// Put the text in place of the characters between the two, as typing over them does.
    Replace(usize, usize, String),
    /// Copy the characters between the two to the clipboard.
    Copy(usize, usize),
    /// Copy them, then delete them.
    Cut(usize, usize),
    /// Paste the clipboard at a place.
    Paste(usize),
}

/// Something a screen reader asked for.
pub enum Act {
    /// Press a button.
    Press(u64),
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
    /// Move the keyboard to one of the window's parts, as clicking it would: the filter box.
    Focus(u64),
    /// Change a field's text, caret or selection, through what typing in it does. The number
    /// is the snapshot's `edit_gen`.
    EditText(u64, u64, TextEdit),
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
}

impl Link {
    pub fn new() -> Self {
        Self { shared: Arc::default(), last: None, epoch: 0, dir: Arc::default(), era: 0, here: Arc::default() }
    }

    /// Hands the server a snapshot if anything changed. Costs nothing while no one listens.
    pub fn update(&mut self, app: &mut App) {
        if !self.shared.is_live() {
            self.last = None;
            return;
        }
        let epoch = self.shared.current_epoch();
        if epoch != self.epoch {
            self.epoch = epoch;
            self.last = None;
        }
        let snap = self.build(app);
        if self.last.as_ref() == Some(&snap) {
            return;
        }
        if let Some((_, tx)) = self.shared.to_server.lock().unwrap().as_ref() {
            let _ = tx.send(ToServer::Snapshot(snap.clone()));
        }
        self.last = Some(snap);
    }

    pub(crate) fn build(&mut self, app: &mut App) -> Snapshot {
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
                Node { id: PLACE0 + i as u64, role: Role::ListItem, name: p.label.clone(), rect, children: Vec::new(), parent: SIDEBAR, selected: p.path == app.dir, enabled: true, showing, description: String::new(), field: None }
            })
            .collect();
        let b = app.px(36.0);
        // As the window's title says it, so Recent and the Trash are called that.
        let title = app.crumb_label(&app.dir);
        let fields = Self::fields(app);
        let node = |id, role, name: &str, rect, children, enabled| Node { id, role, name: name.into(), rect, children, parent: FRAME, selected: false, enabled, showing: true, description: String::new(), field: None };
        // In the order they're drawn: the toolbar, the sidebar, then the list and what's on it.
        let mut frame_kids = vec![BACK];
        frame_kids.extend(fields.iter().filter(|(n, _)| n.id != NAME_FIELD).map(|(n, _)| n.id));
        frame_kids.extend([SIDEBAR, LIST]);
        frame_kids.extend(fields.iter().filter(|(n, _)| n.id == NAME_FIELD).map(|(n, _)| n.id));
        let mut fixed = vec![
            Node { id: FRAME, role: Role::Frame, name: format!("{title} - Gannet"), rect: Rect::new(0, 0, app.w, app.h), children: frame_kids, parent: 0, selected: false, enabled: true, showing: true, description: String::new(), field: None },
            node(BACK, Role::PushButton, "Back", Rect::new(app.px(6.0), 0, b, l.toolbar.h), Vec::new(), app.history.can_go_back()),
            node(SIDEBAR, Role::List, "Places", l.sidebar, places.iter().map(|n| n.id).collect(), true),
            node(LIST, Role::List, "Files", l.list, Vec::new(), true),
        ];
        fixed.extend(places);
        // A field's characters are measured again only when its text changes.
        let mut text_geometry = Vec::new();
        for (n, x0) in &fields {
            let Some(f) = n.field.as_ref() else { continue };
            let kept = self.last.as_ref().and_then(|s| s.text_geometry.iter().find(|(i, _)| *i == n.id)).map(|(_, g)| &g.measured).filter(|m| m.0 == f.text);
            let measured = match kept {
                Some(m) => m.clone(),
                None => Arc::new((f.text.clone(), app.char_offsets(&f.text))),
            };
            let edges = measured.1.iter().map(|o| (x0 + o).round() as i32).collect();
            text_geometry.push((n.id, Arc::new(TextGeometry { measured, edges, line: app.field_line(n.rect) })));
        }
        // The filter box can show what it filters by without being typed in.
        let typing = fields.iter().map(|(n, _)| n.id).find(|&id| id != FILTER || matches!(app.edit, Edit::Filter(_)));
        fixed.extend(fields.into_iter().map(|(n, _)| n));
        let focus = match (typing, rows.entry(app.cursor)) {
            (Some(id), _) => Focus::Node(id),
            (None, Some(e)) if app.cursor_shown => Focus::Row(e),
            _ => Focus::Node(LIST),
        };
        Snapshot { fixed, rows, focus, window_focused: app.has_keyboard, dir: self.dir.clone(), text_geometry, edit_gen: app.edit_gen() }
    }

    /// The text fields showing, each with where its text starts.
    fn fields(app: &mut App) -> Vec<(Node, f32)> {
        let mut out = Vec::new();
        let list = app.layout().list;
        let mut add = |app: &mut App, id, name: String, description: &str, rect: Rect, f: Field| {
            let x0 = app.field_text_x(rect, &f);
            // The row being renamed can be scrolled out of view.
            let showing = id != NAME_FIELD || rect.intersects(list);
            out.push((Node { id, role: Role::Entry, name, rect, children: Vec::new(), parent: FRAME, selected: false, enabled: true, showing, description: description.into(), field: Some(f) }, x0));
        };
        if let Edit::Path(f) = &app.edit {
            let f = f.clone();
            let r = app.path_box();
            add(app, PATH_FIELD, "Path".into(), "Enter a full path, starting with / or ~", r, f);
        }
        if let Some(r) = app.filter_rect() {
            let f = match &app.edit {
                Edit::Filter(f) => f.clone(),
                _ => Field::new(&app.filter),
            };
            // Named for what it does, and described as its empty box says it.
            let (name, hint) = if app.search_below { ("Search", "Search here and below") } else { ("Filter", "Filter this folder") };
            add(app, FILTER, name.into(), hint, r, f);
        }
        let named = match &app.edit {
            Edit::Rename { field, name, .. } => Some((field.clone(), format!("Rename {}", String::from_utf8_lossy(name)))),
            Edit::NewFolder(field) => Some((field.clone(), "New folder name".to_string())),
            _ => None,
        };
        if let Some((f, name)) = named
            && let Some(r) = app.name_field_rect()
        {
            add(app, NAME_FIELD, name, "", r, f);
        }
        out
    }
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
    fn a_fields_characters_are_found_by_place() {
        let g = TextGeometry { measured: Arc::default(), edges: vec![10, 17, 25, 31], line: Rect::new(0, 20, 100, 16) };
        assert_eq!(g.char_rect(1), Rect::new(17, 20, 8, 16));
        assert_eq!(g.char_rect(3), Rect::new(31, 20, 0, 16), "past the end, an empty box");
        assert_eq!(g.char_at(17, 20), Some(1));
        assert_eq!(g.char_at(30, 35), Some(2));
        assert_eq!((g.char_at(31, 25), g.char_at(9, 25), g.char_at(12, 36)), (None, None, None), "beside the text, or below the line");
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
