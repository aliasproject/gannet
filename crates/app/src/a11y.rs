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

use gannet_core::listing::{Kind, Listing, SortKey, Stat};

use crate::app::{App, Edit, Layout};
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
// The list's column headings, the path bar, the sidebar's headings, and the panel of jobs and
// notices. A part of the path is by its place in the path, an eject button by its drive's place
// in the sidebar, a notice by its place among the notices, and a job by its number.
pub const COLUMN_NAME: u64 = 80;
pub const COLUMN_LOCATION: u64 = 81;
pub const COLUMN_SIZE: u64 = 82;
pub const COLUMN_MODIFIED: u64 = 83;
pub const PATH: u64 = 84;
pub const HEADING_PLACES: u64 = 85;
pub const HEADING_PINNED: u64 = 86;
pub const HEADING_DEVICES: u64 = 87;
pub const NOTICES: u64 = 88;
pub const CRUMB0: u64 = 1 << 12;
pub const EJECT0: u64 = 1 << 13;
pub const NOTICE0: u64 = 1 << 14;
pub const DISMISS0: u64 = 1 << 15;
pub const JOB0: u64 = 1 << 32;
pub const STOP0: u64 = 1 << 33;

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
    /// The file list, whose rows are table rows of cells.
    Table,
    ColumnHeader,
    /// A part of the path.
    Link,
    /// A job under way. Its value is in `News::jobs`.
    ProgressBar,
    Panel,
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

/// Where a field's characters are drawn, for a screen reader that shows or magnifies them.
#[derive(Clone, PartialEq, Debug)]
pub struct TextGeometry {
    /// The text, its width, and how far from its start each character's left edge is, and the
    /// last one's right edge. Measured once for each text and size: the field scrolling with
    /// its caret only moves it.
    measured: Arc<(String, f32, Vec<f32>)>,
    /// The left edge of each character, and the right edge of the last.
    pub edges: Vec<i32>,
    /// The line the text is on.
    pub line: Rect,
}

impl TextGeometry {
    /// Where a field's characters are, its text starting at `x0` on `line`. The measuring is
    /// kept from `prev` while the text and its width are the same, so a field scrolling with its
    /// caret only moves them, and a new scale or font measures them again. Measuring up to each
    /// character, as the caret is placed, takes time in proportion to the square of the length,
    /// so a longer text, such as a deep path, is measured a character at a time. That can be a
    /// pixel or two off where letters kern.
    pub fn measure(text: &mut crate::text::Text, f: &Field, x0: f32, line: Rect, prev: Option<&TextGeometry>) -> Self {
        let width = text.measure(&f.text);
        let measured = match prev.map(|g| &g.measured).filter(|m| m.0 == f.text && m.1 == width) {
            Some(m) => m.clone(),
            None => {
                let s = &f.text;
                let mut offsets = vec![0.0];
                if s.len() <= 128 {
                    offsets.extend(s.char_indices().map(|(i, c)| text.measure(&s[..i + c.len_utf8()])));
                } else {
                    let mut x = 0.0;
                    for c in s.chars() {
                        x += text.measure(c.encode_utf8(&mut [0; 4]));
                        offsets.push(x);
                    }
                }
                Arc::new((s.clone(), width, offsets))
            }
        };
        let edges = measured.2.iter().map(|o| (x0 + o).round() as i32).collect();
        TextGeometry { measured, edges, line }
    }

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
    /// The columns, in the order shown, and where each is across a row.
    pub columns: Vec<Col>,
    /// The column the list is sorted by, and whether it's in descending order.
    pub sort: (Column, bool),
}

/// One of the file list's columns.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Column {
    #[default]
    Name,
    /// Where a row of Recent, the Trash or a search is.
    Location,
    Size,
    Modified,
}

impl Column {
    /// Its heading's id.
    pub fn heading(self) -> u64 {
        match self {
            Column::Name => COLUMN_NAME,
            Column::Location => COLUMN_LOCATION,
            Column::Size => COLUMN_SIZE,
            Column::Modified => COLUMN_MODIFIED,
        }
    }
}

/// A column as it's shown: what it is, what its heading says, and where it is across the list.
/// A column too narrow to be drawn has no width.
#[derive(Clone, PartialEq, Debug)]
pub struct Col {
    pub column: Column,
    pub name: &'static str,
    pub x: i32,
    pub w: i32,
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
        self.era == o.era && same_listing && Arc::ptr_eq(&self.order, &o.order) && Arc::ptr_eq(&self.selected, &o.selected) && self.area == o.area && self.row_h == o.row_h && self.scroll == o.scroll && self.columns == o.columns && self.sort == o.sort
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

    /// What a row's cell in a column says, as the list shows it. A folder has no size, and a
    /// file not looked at yet has none of either.
    pub fn cell(&self, entry: u32, column: Column, now: i64) -> String {
        let Some(l) = self.listing.as_deref().filter(|l| (entry as usize) < l.len()) else { return String::new() };
        match (column, l.stat(entry)) {
            (Column::Name, _) => self.name(entry),
            (Column::Location, _) => self.location(entry).unwrap_or_default(),
            (Column::Size, Stat::Known { size, .. }) if !matches!(l.kind(entry), Kind::Dir | Kind::LinkDir) => crate::format::size(size),
            (Column::Modified, Stat::Known { mtime, .. }) => crate::format::date(mtime, now),
            (Column::Modified, Stat::Failed) => "unknown".into(),
            _ => String::new(),
        }
    }

    /// What a row says besides its name: its other cells, in the order shown, so the row read
    /// out as the cursor moves to it sounds as it looks.
    pub fn summary(&self, entry: u32, now: i64) -> String {
        let cells: Vec<String> = self.columns.iter().filter(|c| c.column != Column::Name).map(|c| self.cell(entry, c.column, now)).filter(|t| !t.is_empty()).collect();
        cells.join(", ")
    }

    /// Where a cell is: its column across the row's area.
    pub fn cell_rect(&self, at: usize, col: usize) -> Rect {
        let r = self.rect(at);
        self.columns.get(col).map_or(Rect::new(r.x, r.y, 0, r.h), |c| Rect::new(c.x, r.y, c.w, r.h))
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
    /// Where each field's characters are, by the field's id. A field with none has no boxes.
    pub text_geometry: Vec<(u64, Arc<TextGeometry>)>,
    /// Which field is open, as `App::edit_gen` counts them: a request about one since closed
    /// is dropped.
    pub edit_gen: u64,
    pub news: News,
}

/// What the window has to tell, besides its parts: its notices and the jobs under way. A
/// screen reader hears what's new in it as it comes.
#[derive(Clone, PartialEq, Debug, Default)]
pub struct News {
    /// How many notices have been posted since the window opened, so a new one can be told
    /// from an old one that says the same.
    pub posted: u64,
    pub notices: Vec<String>,
    pub jobs: Vec<JobNews>,
    /// The last few jobs to end, each with whether it did all it was asked.
    pub ended: Vec<(u64, bool)>,
}

/// A job as the panel shows it.
#[derive(Clone, PartialEq, Debug)]
pub struct JobNews {
    pub id: u64,
    pub label: String,
    /// How far along it is, while that's known.
    pub percent: Option<u8>,
    /// What the panel says of its progress: "4.1 MB of 10 MB", or that it's measuring or stopping.
    pub progress: String,
    pub stopping: bool,
}

impl Snapshot {
    pub fn get(&self, id: u64) -> Option<&Node> {
        self.fixed.iter().find(|n| n.id == id)
    }

    /// What to announce on going from `old` to this, each with whether it's urgent: notices
    /// posted, which are, and jobs that finished and the list sorted another way, which aren't.
    /// A job stopped, or one with errors, isn't finished: it posts a notice of its own.
    pub fn news_since(&self, old: &Snapshot) -> Vec<(String, bool)> {
        let mut out = Vec::new();
        if old.rows.era == self.rows.era && old.rows.sort != self.rows.sort {
            let (column, descending) = self.rows.sort;
            if let Some(c) = self.rows.columns.iter().find(|c| c.column == column) {
                out.push((format!("Sorted by {}, {}", c.name.to_lowercase(), if descending { "descending" } else { "ascending" }), false));
            }
        }
        let (was, is) = (&old.news, &self.news);
        let clean = |id: u64| is.ended.iter().any(|&(e, ok)| e == id && ok);
        for j in was.jobs.iter().filter(|j| !is.jobs.iter().any(|n| n.id == j.id) && clean(j.id)) {
            out.push((format!("Finished: {}", j.label), false));
        }
        // Notices are added at the end, so the new ones are the last.
        let fresh = is.posted.saturating_sub(was.posted).min(is.notices.len() as u64) as usize;
        out.extend(is.notices[is.notices.len() - fresh..].iter().map(|t| (t.clone(), true)));
        out
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
    /// Do a part's action: press a button, tick a box, open with an app, sort by a column, or
    /// stop a job.
    Press(u64),
    /// Give a part the keyboard: an entry, or an item of a list other than the file list, which
    /// picks it, as moving to it does with the keyboard.
    Focus(u64),
    /// Open a place in the sidebar.
    OpenPlace(usize),
    /// Scroll the sidebar to show a place.
    ShowPlace(usize),
    /// Go to a part of the path, by its place in it, if the window is still in that folder.
    OpenCrumb(Arc<PathBuf>, usize),
    /// Eject the drive at that place in the sidebar, if it's still the one of that name.
    Eject(usize, String),
    /// Dismiss the notice at that place among them, if it still says that.
    Dismiss(usize, String),
    /// Put the cursor on a row without changing the selection: the row of that key
    /// (`Listing::key`) in that folder. A request about a folder the window has left is dropped,
    /// here and for each request about a row.
    FocusRow(Arc<PathBuf>, Vec<u8>),
    /// Add a row to the selection (true) or take it out, leaving the rest as it is.
    SelectRow(Arc<PathBuf>, Vec<u8>, bool),
    /// Scroll a row into view without moving the cursor.
    ShowRow(Arc<PathBuf>, Vec<u8>),
    /// Change a field's text, caret or selection, through what typing in it does. The number
    /// is the snapshot's `edit_gen`.
    EditText(u64, u64, TextEdit),
    /// Someone started listening: hand over a snapshot.
    Wake,
}

/// What the server is told.
pub enum ToServer {
    /// The window as it is now.
    Snapshot(Box<Snapshot>),
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
    pub fn update(&mut self, app: &mut App) {
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
            let _ = tx.send(ToServer::Snapshot(Box::new(snap.clone())));
        }
        self.last = Some(snap);
    }

    /// Where a field's characters were in the last snapshot handed over, so a window building
    /// its own needn't measure them again while its text is the same.
    pub fn geometry(&self, id: u64) -> Option<&TextGeometry> {
        self.last.as_ref().and_then(|s| s.geometry(id))
    }

    /// No rows, the same each time, so a window without a file list compares as unchanged.
    pub fn no_rows(&self) -> Rows {
        self.no_rows.clone()
    }

    pub(crate) fn build(&mut self, app: &mut App) -> Snapshot {
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
        let sort = app.view_sort();
        let column = match sort.key {
            SortKey::Name => Column::Name,
            SortKey::Size => Column::Size,
            SortKey::Modified => Column::Modified,
        };
        let rows = Rows { era: self.era, listing: app.listing.clone(), order, index, selected, area: l.list, row_h: l.row_h.max(1), scroll: app.scroll, here: self.here.clone(), columns: columns(app, &l), sort: (column, sort.descending) };

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
        let (side, sidebar) = sidebar(app, &l, places);
        let headings: Vec<Node> = rows.columns.iter().map(|c| heading(app, &l, c)).collect();
        let (path, crumbs) = path_bar(app, &l);
        let (panel, panel_parts, news) = panel(app, &l);
        let fields = Self::fields(app);
        let field = |id: u64| fields.iter().any(|n| n.id == id).then_some(id);
        // In the order they're drawn: the toolbar, with the typed path in the path bar's place,
        // the sidebar, the list's headings, the list, the panel below it, and last, as it's on
        // top of the list, the field a row is renamed in.
        let mut frame_kids = vec![BACK];
        frame_kids.extend(field(PATH_FIELD));
        frame_kids.push(PATH);
        frame_kids.extend(field(FILTER));
        frame_kids.push(SIDEBAR);
        frame_kids.extend(headings.iter().map(|n| n.id));
        frame_kids.extend([LIST, NOTICES]);
        frame_kids.extend(field(NAME_FIELD));
        let mut fixed = vec![
            Node::new(FRAME, Role::Frame, format!("{title} - Gannet"), Rect::new(0, 0, app.w, app.h), 0).with_children(frame_kids),
            Node::new(BACK, Role::PushButton, "Back", Rect::new(app.px(6.0), 0, b, l.toolbar.h), FRAME).enabled(app.history.can_go_back()).doing(click("Go back", "<Alt>Left")),
            Node::new(SIDEBAR, Role::List, "Places", l.sidebar, FRAME).with_children(side),
            // What the list says when it has no rows: that the folder's empty, or why it can't be shown.
            // Not while a new folder is being named in it, which takes that message's place.
            Node::new(LIST, Role::Table, "Files", l.list, FRAME).described(app.list_message().filter(|_| !matches!(app.edit, Edit::NewFolder(_))).map(|m| m.0).unwrap_or_default()),
            path,
            panel,
        ];
        fixed.extend(sidebar);
        fixed.extend(headings);
        fixed.extend(crumbs);
        fixed.extend(panel_parts);
        // The filter box can show what it filters by without being typed in.
        let typing = fields.iter().map(|n| n.id).find(|&id| id != FILTER || matches!(app.edit, Edit::Filter(_)));
        fixed.extend(fields);
        let mut focus = match (typing, rows.entry(app.cursor)) {
            // A field being typed in has the keyboard.
            (Some(id), _) => Focus::Node(id),
            (None, Some(e)) if app.cursor_shown => Focus::Row(e),
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
        // Where every field's characters are, the Save dialog's name among them. Each is drawn by
        // `paint_field` in its node's box.
        let mut text_geometry = Vec::new();
        for n in &fixed {
            let Some(f) = n.field.as_ref() else { continue };
            let prev = self.last.as_ref().and_then(|s| s.geometry(n.id));
            let x0 = app.field_text_x(n.rect, f);
            let line = app.field_line(n.rect);
            text_geometry.push((n.id, Arc::new(TextGeometry::measure(&mut app.text, f, x0, line, prev))));
        }
        Snapshot { fixed, rows, focus, window_focused: app.has_keyboard, dir: self.dir.clone(), text_geometry, edit_gen: app.edit_gen(), news }
    }

    /// The text fields of the file manager's own window showing.
    fn fields(app: &mut App) -> Vec<Node> {
        let mut out = Vec::new();
        let list = app.layout().list;
        let mut add = |id, name: String, description: &str, rect: Rect, f: Field| {
            // The row being renamed can be scrolled out of view.
            let showing = id != NAME_FIELD || rect.intersects(list);
            let mut n = Node::new(id, Role::Entry, name, rect, FRAME).described(description).showing(showing);
            n.field = Some(f);
            out.push(n);
        };
        if let Edit::Path(f) = &app.edit {
            let f = f.clone();
            add(PATH_FIELD, "Path".into(), "Enter a full path, starting with / or ~", app.path_box(), f);
        }
        if let Some(r) = app.filter_rect() {
            let f = match &app.edit {
                Edit::Filter(f) => f.clone(),
                _ => Field::new(&app.filter),
            };
            // Named for what it does, and described as its empty box says it.
            let (name, hint) = if app.search_below { ("Search", "Search here and below") } else { ("Filter", "Filter this folder") };
            add(FILTER, name.into(), hint, r, f);
        }
        let named = match &app.edit {
            Edit::Rename { field, name, .. } => Some((field.clone(), format!("Rename {}", String::from_utf8_lossy(name)))),
            Edit::NewFolder(field) => Some((field.clone(), "New folder name".to_string())),
            _ => None,
        };
        if let Some((f, name)) = named
            && let Some(r) = app.name_field_rect()
        {
            add(NAME_FIELD, name, "", r, f);
        }
        out
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
        Snapshot { fixed, rows: self.no_rows(), focus: Focus::Node(PREVIEW), window_focused: app.has_keyboard, dir: self.dir.clone(), text_geometry: Vec::new(), edit_gen: 0, news: News::default() }
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

/// The list's columns, placed as `paint_header` draws them. Recent, the Trash and search
/// results say where each row is, in a column that's left out of the drawing when the window is
/// too narrow for it, but not out of what a screen reader hears.
fn columns(app: &App, l: &Layout) -> Vec<Col> {
    let area = l.list;
    let date_x = area.right() - app.px(14.0) - l.col_date;
    let size_x = date_x - l.col_size;
    let name_x = area.x + app.px(38.0);
    let listing = app.listing.as_deref();
    let trash = listing.is_some_and(|l| l.is_trash());
    let recent = listing.is_some_and(|l| l.is_recent());
    let search = listing.is_some_and(|l| l.search().is_some());
    let origin = (trash || recent || search).then(|| app.origin_column(area, size_x));
    let name_end = origin.flatten().map_or(size_x, |(ox, _)| ox) - app.px(10.0);
    let mut out = vec![Col { column: Column::Name, name: "Name", x: name_x, w: (name_end - name_x).max(0) }];
    if let Some(o) = origin {
        let (x, w) = o.unwrap_or((size_x, 0));
        out.push(Col { column: Column::Location, name: if trash { "Original location" } else { "Location" }, x, w });
    }
    let date = if trash {
        "Deleted"
    } else if recent {
        "Used"
    } else {
        "Modified"
    };
    out.push(Col { column: Column::Size, name: "Size", x: size_x, w: l.col_size });
    out.push(Col { column: Column::Modified, name: date, x: date_x, w: l.col_date });
    out
}

/// A column's heading, which sorts the list by it, as clicking it does. Recent's order is fixed,
/// and nothing sorts by where a row is.
fn heading(app: &App, l: &Layout, c: &Col) -> Node {
    // The Name heading starts left of the names, over their icons, as it's drawn.
    let x = if c.column == Column::Name { l.header.x + app.px(14.0) } else { c.x };
    let n = Node::new(c.column.heading(), Role::ColumnHeader, c.name, Rect::new(x, l.header.y, c.x + c.w - x, l.header.h), FRAME).showing(c.w > 0);
    let action = match c.column {
        Column::Name => click("Sort by name", "<Control>1"),
        Column::Size => click("Sort by size", "<Control>2"),
        Column::Modified => click("Sort by date", "<Control>3"),
        Column::Location => return n,
    };
    if app.is_recent_view() { n } else { n.doing(action) }
}

/// The sidebar's children, its headings among its places, and the parts in it: the places,
/// the headings, and the eject buttons of drives that can be taken out, each inside its drive.
fn sidebar(app: &App, l: &Layout, mut places: Vec<Node>) -> (Vec<u64>, Vec<Node>) {
    let heads = app.sidebar_headings();
    let heading_h = app.px(crate::app::SIDEBAR_HEADING);
    let in_view = |r: Rect| r.y + r.h > l.sidebar.y && r.y < l.sidebar.y + l.sidebar.h;
    let mut side = Vec::new();
    let mut extra = Vec::new();
    for (i, n) in places.iter_mut().enumerate() {
        if let Some(&(_, label)) = heads.iter().find(|(at, _)| *at == i) {
            let id = match label {
                "Pinned" => HEADING_PINNED,
                "Devices" => HEADING_DEVICES,
                _ => HEADING_PLACES,
            };
            let r = Rect::new(l.sidebar.x, app.place_top(i) - heading_h, l.sidebar.w, heading_h);
            side.push(id);
            extra.push(Node::new(id, Role::Heading, label, r, SIDEBAR).showing(in_view(r)));
        }
        side.push(n.id);
        let Some(r) = app.eject_rect(i) else { continue };
        let Some(v) = app.places.get(i).and_then(|p| p.volume.as_ref()) else { continue };
        let id = EJECT0 + i as u64;
        n.children = vec![id];
        extra.push(Node::new(id, Role::PushButton, format!("Eject {}", n.name), r, n.id).showing(n.showing).enabled(!app.drives_busy.contains(&v.block)).doing(click("Eject the drive", "")));
    }
    places.extend(extra);
    (side, places)
}

/// The path bar: a list of links, one for each folder of the path, and the parts in it. The
/// parts cut off its left aren't showing, and nor is any while the path is being typed.
fn path_bar(app: &mut App, l: &Layout) -> (Node, Vec<Node>) {
    let typing = matches!(app.edit, Edit::Path(_));
    let laid = if typing { Vec::new() } else { app.crumbs() };
    let h = (l.toolbar.h - app.px(16.0)).max(0);
    let crumbs: Vec<Node> = app.crumb_parts().into_iter().enumerate().map(|(i, (_, path))| {
            let at = laid.iter().find(|(_, _, p)| *p == path).map(|&(x, w, _)| Rect::new(x, app.px(8.0), w, h));
            Node::new(CRUMB0 + i as u64, Role::Link, app.crumb_label(&path), at.unwrap_or_default(), PATH).selected(path == app.dir).showing(at.is_some() && !typing).doing(Action::new("jump", "Go to this folder", ""))
        })
        .collect();
    let x = laid.first().map_or(l.toolbar.x, |c| c.0);
    let right = laid.last().map_or(x, |c| c.0 + c.1);
    let bar = Node::new(PATH, Role::List, "Path", Rect::new(x, 0, right - x, l.toolbar.h), FRAME).showing(!typing).with_children(crumbs.iter().map(|n| n.id).collect());
    (bar, crumbs)
}

/// The panel above the status bar, its parts, and the news a screen reader hears of: each job
/// under way, with its Stop button, then each notice, with its Dismiss button. Only the first
/// four of each are drawn.
fn panel(app: &App, l: &Layout) -> (Node, Vec<Node>, News) {
    let row = app.px(30.0);
    let mut parts = Vec::new();
    let mut jobs = Vec::new();
    let place = |k: usize| Rect::new(l.notices.x, l.notices.y + k as i32 * row, l.notices.w, row);
    let close = |k: usize| Rect::new(l.notices.right() - row, l.notices.y + k as i32 * row, row, row);
    for (k, j) in app.visible_jobs().enumerate() {
        let p = &j.job.progress;
        let (total, done) = (p.total_bytes.load(Ordering::Relaxed), p.done_bytes.load(Ordering::Relaxed));
        let (items, done_items) = (p.total_items.load(Ordering::Relaxed), p.done_items.load(Ordering::Relaxed));
        let scanning = p.scanning.load(Ordering::Relaxed);
        let stopping = j.job.is_cancelled();
        // By bytes, or by items when there are no bytes to count.
        let percent = if scanning { None } else { (done.min(total) * 100).checked_div(total).or_else(|| (done_items.min(items) * 100).checked_div(items)).map(|p| p as u8) };
        // As the panel says it.
        let progress = if stopping {
            "stopping".to_string()
        } else if scanning {
            "measuring".to_string()
        } else if total > 0 {
            format!("{} of {}", crate::format::size(done), crate::format::size(total))
        } else {
            String::new()
        };
        let shown = k < 4;
        parts.push(Node::new(JOB0 + j.job.id, Role::ProgressBar, j.job.label.clone(), place(k), NOTICES).showing(shown));
        parts.push(Node::new(STOP0 + j.job.id, Role::PushButton, "Stop", close(k), NOTICES).described(j.job.label.clone()).showing(shown).enabled(!stopping).doing(click("Stop the job", "")));
        jobs.push(JobNews { id: j.job.id, label: j.job.label.clone(), percent, progress, stopping });
    }
    let first = jobs.len().min(4);
    for (i, text) in app.notices.iter().enumerate() {
        let shown = i < 4;
        parts.push(Node::new(NOTICE0 + i as u64, Role::Label, text.clone(), place(first + i), NOTICES).showing(shown));
        parts.push(Node::new(DISMISS0 + i as u64, Role::PushButton, "Dismiss", close(first + i), NOTICES).described(text.clone()).showing(shown).doing(click("Dismiss the notice", "")));
    }
    let panel = Node::new(NOTICES, Role::Panel, "Notices", l.notices, FRAME).showing(l.notices.h > 0).with_children(parts.iter().map(|n| n.id).collect());
    let news = News { posted: app.notices_posted, notices: app.notices.clone(), jobs, ended: app.jobs_ended.clone() };
    (panel, parts, news)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(n: usize, scroll: f64) -> Rows {
        let order: Vec<u32> = (0..n as u32).rev().collect();
        Rows { era: 1, listing: None, order: Arc::new(order), index: Arc::default(), selected: Arc::new(vec![0b100]), area: Rect::new(10, 100, 300, 200), row_h: 20, scroll, ..Rows::default() }
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
