//! The window's state and everything the user can do to it. No Wayland in here: input
//! arrives as plain values and requests for the platform go out through `cmds`.

use std::collections::HashMap;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gannet_core::clipboard;
use gannet_core::history::{History, Stop, parent_stop};
use gannet_core::listing::{Kind, LoadEvent, LoadHandle, LoadRequest, Listing, Loaded, SortKey, SortSpec, Stat, Stater, DISKS_GIVE_UP, TrashSpec, sort_entries, spawn_load_watching, visible_rows};
use gannet_core::mounts::Mount;
use gannet_core::trash;
use gannet_core::ops::{self, Answer, Choice, Conflict, Engine, Job, Request, Undo};
use gannet_core::watch::Watch;
use gannet_core::places::{Place, PlaceKind};
use gannet_core::theme::{Colors, Source};
use rustix::fd::OwnedFd;
use xkbcommon::xkb::keysyms as k;

use crate::canvas::Rect;
use crate::field::Field;
use crate::icons::Icons;
use crate::preview::{self, Preview};
use crate::text::Text;
use crate::wl::{Cursor, Input, Mods};

pub const BTN_LEFT: u32 = 0x110;
pub const BTN_RIGHT: u32 = 0x111;
pub const BTN_SIDE: u32 = 0x113;
pub const BTN_EXTRA: u32 = 0x114;

const DOUBLE_CLICK: Duration = Duration::from_millis(400);
const TYPE_AHEAD_RESET: Duration = Duration::from_millis(900);
const HOLD_FOR_LOAD: Duration = Duration::from_millis(16);
const JOB_SHOW_AFTER: Duration = Duration::from_millis(200);
const UNDO_DEPTH: usize = 20;
const DIALOG_GUARD: Duration = Duration::from_millis(400);
const CACHE_ENTRIES: usize = 8;
const CACHE_TOTAL_ROWS: usize = 300_000;
/// Below this, filtering on the UI thread takes well under a frame.
const SYNC_DERIVE_LIMIT: usize = 20_000;

/// Wakes the main loop from worker threads.
#[derive(Clone)]
pub struct Hub {
    pub tx: Sender<Msg>,
    pub wake: Arc<OwnedFd>,
}

impl Hub {
    pub fn send(&self, m: Msg) {
        if self.tx.send(m).is_ok() {
            let _ = rustix::io::write(&*self.wake, &1u64.to_ne_bytes());
        }
    }
}

pub enum Msg {
    Load(LoadEvent),
    Stats(u64),
    Derived { generation: u64, listing: Arc<Listing>, sorted: Arc<Vec<u32>>, rows: Vec<u32>, sort: SortSpec },
    Font(PathBuf, u32),
    PrimaryFont(PathBuf, u32),
    DesktopDark(bool),
    Notice(String),
    Op(ops::Event),
    /// `data` is `None` if it was too big to trust.
    Clipboard(Option<Vec<u8>>, String, Paste),
    Preview(preview::Event),
    /// A file list dropped from a Wayland drag, read and looked at off the main thread.
    Dropped(Dropped),
    DragSource { drag: u64, facts: Option<(u64, bool)> },
    DragDevice { drag: u64, path: PathBuf, device: Option<u64> },
}

pub struct Dropped {
    pub paths: Vec<PathBuf>,
    pub dest: PathBuf,
    pub trash: bool,
    /// What the other program allows.
    pub can_copy: bool,
    pub can_move: bool,
    /// What a drag from another Gannet window does, worked out at the drop.
    pub decided: Option<DropAction>,
    /// Every entry was a local path.
    pub all_local: bool,
}

/// What a clipboard read is for.
#[derive(Clone, Debug, PartialEq)]
pub enum Paste {
    /// Into the folder that was showing when the paste was asked for. The other program
    /// might take its time, and by then a different folder could be up.
    Files { dest: PathBuf },
    /// Into the field that was open, identified by its `edit_gen`.
    IntoField { edit: u64 },
}

/// What the app asks of the platform.
pub enum Cmd {
    Title(String),
    Cursor(Cursor),
    SetClipboard(Vec<(&'static str, Vec<u8>)>),
    ClearClipboard,
    ReadClipboard(Paste),
    /// Hands the drag over to the compositor, which then carries it to other programs.
    StartDrag { formats: Vec<(&'static str, Vec<u8>)>, marker: Option<clipboard::DragMarker>, move_only: bool, icon: Option<crate::wl::DragIcon> },
    CancelDrag,
    AcceptDrop(bool),
    ReadDrop { dest: PathBuf, trash: bool, decided: Option<DropAction> },
    FinishDrop,
    AbandonDrop,
    Quit,
}

/// A running operation shown in the panel once it's lasted long enough to matter.
pub struct JobView {
    pub job: Job,
    pub started: Instant,
    ends_undo: bool,
    selection_epoch: u64,
}

/// A question that has to be answered before anything else happens.
pub enum Dialog {
    Conflict { conflict: Conflict, apply_to_all: bool, at: usize },
    /// `empty` is for emptying the Trash, which words the question differently.
    ConfirmDelete { paths: Vec<PathBuf>, at: usize, empty: bool },
    ConfirmQuit { at: usize },
}

/// One dialog button: its label and the key that presses it.
pub struct DialogButton {
    pub label: &'static str,
    pub key: &'static str,
    pub danger: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hit {
    None,
    Back,
    Forward,
    Up,
    EmptyTrash,
    PathBar,
    Crumb(usize),
    FilterBox,
    Place(usize),
    Header(SortKey),
    Row(usize),
    ListEmpty,
    Scrollbar,
    NoticeClose(usize),
    MenuItem(usize),
    Outside,
    JobCancel(usize),
    DialogButton(usize),
    DialogCheck,
    PreviewBody,
}

/// How far the pointer has to travel before a press on a row becomes a drag, in logical pixels.
const DRAG_START: f64 = 6.0;
/// How long pointer events can keep arriving after a drag is handed to the compositor.
const WAYLAND_DRAG_GRACE: Duration = Duration::from_millis(300);

/// A drag of files inside the window. It's pointer state and not a Wayland drag, because
/// compositors may keep keys from the app during one, and Escape and the keys held at the
/// drop have to work.
pub(crate) enum FileDrag {
    Pressed {
        at: (i32, i32),
        row: usize,
        /// What the row was, so a reload or a new folder can't redirect the press.
        dir: PathBuf,
        name: Vec<u8>,
        /// The press landed on a row in a multi-selection. If this doesn't become a drag,
        /// the release narrows the selection to that row, as the press normally would.
        narrow: bool,
        /// The press was beside the row's name, so a drag from here draws a box instead. This is
        /// what was selected, and where in the whole list the press was, as the list can scroll.
        band: Option<(SavedSelection, f64)>,
    },
    Active(ActiveDrag),
    Band(Band),
}

/// The selection as it was before a press, to build on or go back to.
pub(crate) struct SavedSelection {
    selected: Vec<u64>,
    count: usize,
    cursor: usize,
    anchor: usize,
    shown: bool,
    /// Ctrl or Shift was held, so what a box touches adds to this.
    add: bool,
}

/// A box being dragged over the rows to select them.
pub(crate) struct Band {
    at: (i32, i32),
    /// Still just a press, until the pointer has moved far enough.
    pub(crate) active: bool,
    /// Where it started, measured down the whole list so that it stays put when that scrolls.
    start_y: f64,
    saved: SavedSelection,
    /// The rows it touches, first and last.
    range: Option<(usize, usize)>,
    /// What's on screen, in the list.
    pub(crate) rect: Rect,
    /// When to scroll next, with the pointer past the end of a list that has further to go.
    scroll_at: Option<Instant>,
}

const BAND_SCROLL_EVERY: Duration = Duration::from_millis(16);

pub(crate) struct ActiveDrag {
    pub paths: Vec<PathBuf>,
    /// The folder they're from. Rows and places are judged against the folder showing.
    dir: PathBuf,
    /// Told which folders to look at. Asking a disk can hang, so that happens on a thread,
    /// and until the answer comes, the drop rule copies.
    probe: Sender<PathBuf>,
    id: u64,
    /// The source folder's device and whether its items can be deleted.
    source: Option<(u64, bool)>,
    /// The device of each folder hovered so far. `None` means it couldn't be read.
    devices: HashMap<PathBuf, Option<u64>>,
    pub target: Option<DropTarget>,
    pub label: Rect,
    /// What the pointer carries, drawn like the row it came from.
    pub ghost: DragGhost,
    pub ghost_at: Rect,
    /// The compositor has the drag now, because the pointer left the window.
    wayland: bool,
    /// When that happened. Pointer events can still be on their way for a moment after.
    handed_over: Option<Instant>,
    /// A Wayland drag that has come back over this window.
    inside: bool,
    /// The pointer has really moved since it came back. See `drag_entered`.
    moved_inside: bool,
    accepting: bool,
}

impl ActiveDrag {
    /// Out over other programs, with nothing of it in this window.
    #[cfg(feature = "bench")]
    pub fn is_out(&self) -> bool {
        self.wayland && !self.inside
    }
}

#[derive(Clone)]
pub(crate) struct DragGhost {
    pub name: Vec<u8>,
    pub kind: Kind,
    pub count: usize,
}

/// A Wayland drag over this window, from another program or window.
pub(crate) struct IncomingDrag {
    files: bool,
    pub can_copy: bool,
    pub can_move: bool,
    /// Set when it comes from another Gannet window.
    marker: Option<clipboard::DragMarker>,
    /// Asks a thread for each folder's device. Only for a drag with a marker.
    probe: Option<Sender<PathBuf>>,
    id: u64,
    /// The device of each folder hovered so far. `None` means it couldn't be read.
    devices: HashMap<PathBuf, Option<u64>>,
    /// The pointer has really moved since the drag arrived. See `drag_entered`.
    moved: bool,
    pub target: Option<DropTarget>,
    pub label: Rect,
    accepting: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct DropTarget {
    pub hit: Hit,
    pub dest: PathBuf,
    trash: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DropAction {
    Move,
    Copy,
    Link,
    Trash,
}

#[derive(Clone, Debug, PartialEq, Eq, Copy)]
pub enum Action {
    Open,
    Preview,
    ToggleHidden,
    SortBy(SortKey),
    Reload,
    NewWindow,
    Cut,
    Copy,
    Paste,
    Rename,
    Trash,
    DeleteForGood,
    NewFolder,
    Undo,
    Restore,
    EmptyTrash,
}

pub struct MenuItem {
    pub label: &'static str,
    pub keys: &'static str,
    pub action: Action,
    pub enabled: bool,
}

pub struct Menu {
    pub items: Vec<MenuItem>,
    pub x: i32,
    pub y: i32,
    pub at: usize,
}

/// Which text field has the keyboard, if any.
#[derive(Clone, Debug, PartialEq)]
pub enum Edit {
    None,
    Path(Field),
    Filter(Field),
    Rename { initial: String, name: Vec<u8>, path: PathBuf, field: Field },
    NewFolder(Field),
}

pub struct Layout {
    pub toolbar: Rect,
    pub sidebar: Rect,
    pub header: Rect,
    pub list: Rect,
    pub notices: Rect,
    pub status: Rect,
    pub row_h: i32,
    pub col_size: i32,
    pub col_date: i32,
}

pub struct App {
    pub hub: Hub,
    pub colors: Colors,
    pub theme_source: Source,
    pub text: Text,
    pub icons: Icons,
    pub w: i32,
    pub h: i32,
    pub scale: f64,
    pub dirty: Rect,
    pub cmds: Vec<Cmd>,

    pub dir: PathBuf,
    pub listing: Option<Arc<Listing>>,
    pub sorted: Arc<Vec<u32>>,
    pub rows: Vec<u32>,
    pub sort: SortSpec,
    pub show_hidden: bool,
    pub filter: String,
    pub loading: bool,
    pub load_error: Option<String>,
    load: Option<LoadHandle>,
    load_gen: u64,
    derive_gen: u64,
    derive_cancel: Arc<AtomicBool>,
    req_hidden: bool,
    req_filter: String,
    stater: Option<Stater>,
    stats_range: (usize, usize),
    pending_cursor: Option<Vec<u8>>,
    pub hold_paint: Option<Instant>,
    pub reload_at: Option<Instant>,
    load_started: Instant,
    last_load_cost: Duration,

    pub cursor: usize,
    pub cursor_shown: bool,
    anchor: usize,
    selected: Vec<u64>,
    pub sel_count: usize,
    pub scroll: f64,

    pub history: History,
    cache: Vec<Cached>,
    pub places: Vec<Place>,
    pub edit: Edit,
    pub menu: Option<Menu>,
    pub notices: Vec<String>,

    pub engine: Engine,
    pub trash_dir: PathBuf,
    pub jobs: Vec<JobView>,
    pub dialog: Option<Dialog>,
    pending_conflicts: Vec<Conflict>,
    pub undo: Vec<Undo>,
    pending_select: Vec<Vec<u8>>,
    /// Goes up whenever the user changes the selection, so a finishing job can tell.
    selection_epoch: u64,
    edit_gen: u64,
    paste_in_flight: bool,
    /// When an unasked-for dialog appeared. Input right after that was meant for
    /// whatever was there before, so it doesn't answer the dialog.
    dialog_guard: Option<Instant>,
    /// What a pending cut will move, for dimming those rows.
    pub cut_paths: Vec<PathBuf>,
    quit_when_idle: bool,

    type_ahead: String,
    type_ahead_at: Instant,
    pub pointer: (i32, i32),
    pub hover: Hit,
    last_click: Option<(Instant, Vec<u8>)>,
    drag_scroll: Option<i32>,
    pub(crate) file_drag: Option<FileDrag>,
    pub(crate) incoming: Option<IncomingDrag>,
    /// The job whose result decides how a drop is answered, and whether the list was all local.
    drop_job: Option<(u64, bool)>,
    /// The cursor last asked for, so the same one isn't asked for again.
    pointer_cursor: Cursor,
    next_drag: u64,
    pub mods_now: Mods,
    /// Whether this window has the keyboard, so `mods_now` is worth believing during a drag
    /// from another window. Many compositors keep the keyboard from a window during one.
    has_keyboard: bool,
    pub watch: Option<Arc<Watch>>,
    /// A re-sort is on its way, so a lighter request mustn't replace it.
    resort_pending: bool,
    /// A change arrived during a load, so another load follows it.
    reload_after_load: bool,
    stater_gen: u64,
    /// The folder named on the command line might be a file. We only learn that by trying.
    first_load: bool,
    pub slow_read: Option<Duration>,
    /// Test seam: the mount table the Trash view looks for other disks' Trash folders in.
    pub trash_mounts: Option<Vec<Mount>>,

    pub preview: Option<Preview>,
    pub(crate) preview_gen: u64,
    /// Where other programs keep thumbnails.
    pub cache_home: Option<PathBuf>,
    pub slow_preview: Option<Duration>,
    pub(crate) key_is_repeat: bool,
}

struct Cached {
    listing: Arc<Listing>,
    sorted: Arc<Vec<u32>>,
    sort: SortSpec,
}

const FULL: Rect = Rect::new(0, 0, i32::MAX / 2, i32::MAX / 2);

impl App {
    #[allow(clippy::too_many_arguments)]
    pub fn new(hub: Hub, engine: Engine, trash_dir: PathBuf, colors: Colors, theme_source: Source, text: Text, places: Vec<Place>, dir: PathBuf, w: i32, h: i32, scale: f64) -> Self {
        Self {
            hub,
            colors,
            theme_source,
            text,
            icons: Icons::default(),
            w,
            h,
            scale,
            dirty: FULL,
            cmds: Vec::new(),
            dir,
            listing: None,
            sorted: Arc::new(Vec::new()),
            rows: Vec::new(),
            sort: SortSpec::default(),
            show_hidden: false,
            filter: String::new(),
            loading: false,
            load_error: None,
            load: None,
            load_gen: 0,
            derive_gen: 0,
            derive_cancel: Arc::new(AtomicBool::new(false)),
            req_hidden: false,
            req_filter: String::new(),
            stater: None,
            stats_range: (0, 0),
            pending_cursor: None,
            hold_paint: None,
            reload_at: None,
            load_started: Instant::now(),
            last_load_cost: Duration::ZERO,
            cursor: 0,
            cursor_shown: false,
            anchor: 0,
            selected: Vec::new(),
            sel_count: 0,
            scroll: 0.0,
            history: History::default(),
            cache: Vec::new(),
            places,
            edit: Edit::None,
            menu: None,
            notices: Vec::new(),
            engine,
            trash_dir,
            jobs: Vec::new(),
            dialog: None,
            pending_conflicts: Vec::new(),
            undo: Vec::new(),
            pending_select: Vec::new(),
            selection_epoch: 0,
            edit_gen: 0,
            paste_in_flight: false,
            dialog_guard: None,
            cut_paths: Vec::new(),
            quit_when_idle: false,
            type_ahead: String::new(),
            type_ahead_at: Instant::now(),
            pointer: (-1, -1),
            hover: Hit::None,
            last_click: None,
            drag_scroll: None,
            file_drag: None,
            incoming: None,
            drop_job: None,
            pointer_cursor: Cursor::Default,
            next_drag: 0,
            mods_now: Mods::default(),
            has_keyboard: false,
            watch: None,
            resort_pending: false,
            reload_after_load: false,
            stater_gen: 0,
            first_load: true,
            slow_read: None,
            trash_mounts: None,
            preview: None,
            preview_gen: 0,
            cache_home: None,
            slow_preview: None,
            key_is_repeat: false,
        }
    }

    pub fn px(&self, v: f64) -> i32 {
        (v * self.scale).round() as i32
    }

    pub fn invalidate(&mut self, r: Rect) {
        self.dirty = self.dirty.union(r);
    }

    pub fn invalidate_all(&mut self) {
        self.dirty = FULL;
    }

    pub fn resized(&mut self, w: i32, h: i32, scale: f64) {
        let rescale = (scale - self.scale).abs() > f64::EPSILON;
        self.w = w;
        self.h = h;
        if rescale && self.scale > 0.0 {
            self.scroll *= scale / self.scale;
        }
        self.scale = scale;
        self.text.set_size(14.0 * scale as f32, scale < 1.5);
        self.clamp_scroll();
        self.stats_range = (0, 0);
        self.request_stats();
        self.preview_rescale();
        self.invalidate_all();
    }

    pub fn layout(&self) -> Layout {
        let (w, h) = (self.w, self.h);
        let bar = self.px(44.0);
        let status = self.px(26.0);
        let side = self.px(190.0).min(w / 3);
        let header = self.px(28.0);
        let notice_h = self.px(30.0) * self.panel_rows() as i32;
        let top = bar + header;
        let bottom = h - status - notice_h;
        let list_w = w - side;
        Layout {
            toolbar: Rect::new(0, 0, w, bar),
            sidebar: Rect::new(0, bar, side, h - bar - status),
            header: Rect::new(side, bar, list_w, header),
            list: Rect::new(side, top, list_w, (bottom - top).max(0)),
            notices: Rect::new(side, bottom, list_w, notice_h),
            status: Rect::new(0, h - status, w, status),
            row_h: self.px(26.0),
            col_size: self.px(90.0),
            col_date: self.px(130.0),
        }
    }

    /// How many rows the panel above the status bar takes.
    pub fn panel_rows(&self) -> usize {
        self.visible_jobs().count().min(4) + self.notices.len().min(4)
    }

    /// Quick operations never show, so the layout doesn't flicker.
    pub fn visible_jobs(&self) -> impl Iterator<Item = &JobView> {
        self.jobs.iter().filter(|j| j.started.elapsed() >= JOB_SHOW_AFTER)
    }

    // ---- Loading ----

    fn row_capacity(&self) -> usize {
        let l = self.layout();
        (l.list.h / l.row_h.max(1)).max(1) as usize + 2
    }

    /// Goes to a folder as a new step in history.
    pub fn navigate(&mut self, dir: PathBuf, cursor_name: Option<Vec<u8>>) {
        let from = self.current_stop();
        self.history.visit(from);
        self.go(Stop { dir, cursor_name });
    }

    fn current_stop(&self) -> Stop {
        // With no cursor showing there's nothing to come back to.
        let cursor_name = if self.cursor_shown { self.listing.as_ref().and_then(|l| self.rows.get(self.cursor).map(|&i| l.key(i).into_owned())) } else { None };
        Stop { dir: self.dir.clone(), cursor_name }
    }

    /// Shows a folder now if it's cached, and reads it either way.
    pub fn go(&mut self, stop: Stop) {
        self.rows_changing();
        self.dir = stop.dir;
        self.pending_cursor = stop.cursor_name;
        self.filter.clear();
        self.edit = Edit::None;
        self.edit_gen += 1;
        self.pending_select.clear();
        self.menu = None;
        self.load_error = None;
        self.clear_selection();
        self.cursor = 0;
        self.anchor = 0;
        self.cursor_shown = false;
        self.scroll = 0.0;
        self.reload_at = None;
        self.reload_after_load = false;
        self.resort_pending = false;
        self.derive_cancel.store(true, Ordering::Relaxed);
        self.derive_gen += 1;
        self.cmds.push(Cmd::Title(self.crumb_label(&self.dir)));
        if let Some(i) = self.cache.iter().position(|c| c.listing.dir == self.dir) {
            let c = self.cache.remove(i);
            self.show_cached(c);
        } else {
            self.listing = None;
            self.sorted = Arc::new(Vec::new());
            self.rows.clear();
            self.stater = None;
            self.hold_paint = Some(Instant::now() + HOLD_FOR_LOAD);
        }
        self.start_load();
        self.invalidate_all();
    }

    fn show_cached(&mut self, c: Cached) {
        let same_sort = c.sort == self.sort;
        self.listing = Some(c.listing.clone());
        self.sorted = c.sorted;
        self.rows = visible_rows(&c.listing, &self.sorted, self.show_hidden, &self.filter);
        self.selected = vec![0; c.listing.len().div_ceil(64)];
        self.new_stater(c.listing);
        self.place_pending_cursor(true, false);
        self.request_stats();
        if !same_sort {
            self.rederive(true);
        }
    }

    #[cfg(feature = "bench")]
    pub fn load_elapsed(&self) -> Duration {
        self.load_started.elapsed()
    }

    pub fn reload(&mut self) {
        self.start_load();
        self.invalidate(self.layout().status);
    }

    fn start_load(&mut self) {
        if let Some(h) = self.load.take() {
            h.cancel();
        }
        self.load_gen += 1;
        self.loading = true;
        self.load_started = Instant::now();
        self.req_hidden = self.show_hidden;
        self.req_filter = self.filter.clone();
        let row_h = self.layout().row_h.max(1);
        let req = LoadRequest {
            dir: self.dir.clone(),
            generation: self.load_gen,
            sort: self.sort,
            show_hidden: self.show_hidden,
            filter: self.filter.clone(),
            first_row: (self.scroll / row_h as f64) as usize,
            row_count: self.row_capacity(),
            slow_read: self.slow_read,
            slow_stat: None,
            trash: self.is_trash_view().then(|| TrashSpec { home_trash: self.home_trash(), uid: rustix::process::geteuid().as_raw(), mounts: self.trash_mounts.clone(), disks_give_up: DISKS_GIVE_UP }),
        };
        let hub = self.hub.clone();
        self.load = Some(spawn_load_watching(req, self.watch.clone(), move |e| hub.send(Msg::Load(e))));
    }

    /// A change on disk: reload soon, but less often when reloading is expensive.
    pub fn folder_changed(&mut self) {
        if self.loading {
            self.reload_after_load = true;
        } else if self.reload_at.is_none() {
            let wait = (self.last_load_cost * 5).clamp(Duration::from_millis(50), Duration::from_secs(5));
            self.reload_at = Some(Instant::now() + wait);
        }
    }

    pub fn on_timer(&mut self, now: Instant) {
        if self.reload_at.is_some_and(|t| t <= now) {
            self.reload_at = None;
            self.reload();
        }
        if self.hold_paint.is_some_and(|t| t <= now) {
            self.hold_paint = None;
        }
        self.band_on_timer(now);
        self.preview_on_timer(now);
        if self.jobs.iter().any(|j| {
            let due = j.started + JOB_SHOW_AFTER;
            due <= now && due + Duration::from_millis(50) > now
        }) {
            self.clamp_scroll();
            self.invalidate_all();
        }
    }

    pub fn next_deadline(&self) -> Option<Instant> {
        // A running job that hasn't shown yet needs one wake-up, when it's due to.
        let job = self.jobs.iter().map(|j| j.started + JOB_SHOW_AFTER).filter(|&t| t > Instant::now()).min();
        let band = if let Some(FileDrag::Band(b)) = &self.file_drag { b.scroll_at } else { None };
        [self.reload_at, self.hold_paint, job, self.preview_deadline(), band].into_iter().flatten().min()
    }

    fn on_loaded(&mut self, l: Loaded) {
        if l.generation != self.load_gen {
            return;
        }
        self.hold_paint = None;
        self.first_load = false;
        let same_dir = self.listing.as_ref().is_some_and(|o| o.dir == l.listing.dir);
        let old = self.listing.clone();
        // A partial reload of a folder we can already show would make rows and the
        // selection vanish for a moment. The complete listing stays up until the new one is.
        if same_dir && !l.settled && old.as_ref().is_some_and(|o| o.complete) {
            return;
        }
        // Whatever a finished operation wants selected, now that it's in the listing.
        let select = if l.settled { std::mem::take(&mut self.pending_select) } else { Vec::new() };
        if l.settled {
            self.loading = false;
            self.last_load_cost = self.load_started.elapsed();
            if std::mem::take(&mut self.reload_after_load) {
                self.folder_changed();
            }
        }
        self.load_error = l.listing.read_error.clone();
        let name_at = |row: usize, rows: &[u32]| old.as_ref().filter(|_| same_dir).and_then(|o| rows.get(row).map(|&i| o.key(i).into_owned()));
        let picks: Vec<Vec<u8>> = match &old {
            Some(o) if same_dir && self.sel_count > 0 => self.rows.iter().filter(|&&i| self.is_selected(i)).map(|&i| o.key(i).into_owned()).collect(),
            _ => Vec::new(),
        };
        self.rows_changing();
        let (keep_cursor, keep_anchor) = (name_at(self.cursor, &self.rows), name_at(self.anchor, &self.rows));
        self.listing = Some(l.listing.clone());
        self.sorted = Arc::new(l.sorted);
        self.rows = l.rows;
        self.selected = vec![0; l.listing.len().div_ceil(64)];
        self.sel_count = 0;
        if !picks.is_empty() {
            let lookup: std::collections::HashMap<std::borrow::Cow<[u8]>, u32> = (0..l.listing.len() as u32).map(|i| (l.listing.key(i), i)).collect();
            for p in &picks {
                if let Some(&i) = lookup.get(p.as_slice()) {
                    self.set_selected(i, true);
                }
            }
        }
        self.new_stater(l.listing.clone());
        // Same folder: everything stays where it was, by name. A cursor that wasn't
        // showing stays hidden, and the view doesn't jump.
        let row_of = |name: &Option<Vec<u8>>, rows: &[u32]| name.as_ref().and_then(|n| rows.iter().position(|&i| *l.listing.key(i) == *n.as_slice()));
        if same_dir {
            let last = self.rows.len().saturating_sub(1);
            self.cursor = row_of(&keep_cursor, &self.rows).unwrap_or(self.cursor.min(last));
            self.anchor = row_of(&keep_anchor, &self.rows).unwrap_or(self.anchor.min(last));
        }
        self.place_pending_cursor(true, l.settled);
        if !select.is_empty() {
            self.clear_selection();
            let first = select.first().cloned();
            let lookup: std::collections::HashMap<std::borrow::Cow<[u8]>, u32> = (0..l.listing.len() as u32).map(|i| (l.listing.key(i), i)).collect();
            for name in &select {
                if let Some(&i) = lookup.get(name.as_slice()) {
                    self.set_selected(i, true);
                }
            }
            if let Some(row) = first.and_then(|n| self.rows.iter().position(|&i| *l.listing.key(i) == *n.as_slice())) {
                self.cursor = row;
                self.anchor = row;
                self.cursor_shown = true;
                self.ensure_visible();
            }
        }
        // The entry being renamed is gone, so there's nothing left to rename.
        if let Edit::Rename { name, .. } = &self.edit
            && l.settled
            && l.listing.find(name).is_none()
        {
            self.edit = Edit::None;
        }
        // Settings changed while this was loading: it was built for the old ones.
        if l.sort != self.sort {
            self.rederive(true);
        } else if self.req_hidden != self.show_hidden || self.req_filter != self.filter {
            self.rederive(false);
        }
        if l.settled && l.listing.complete {
            self.remember(l.listing, l.sort);
        }
        self.clamp_scroll();
        self.stats_range = (0, 0);
        self.request_stats();
        self.invalidate_all();
    }

    fn remember(&mut self, listing: Arc<Listing>, sort: SortSpec) {
        if !listing.complete {
            return;
        }
        self.cache.retain(|c| c.listing.dir != listing.dir);
        self.cache.push(Cached { listing, sorted: self.sorted.clone(), sort });
        while self.cache.len() > CACHE_ENTRIES || self.cache.iter().map(|c| c.listing.len()).sum::<usize>() > CACHE_TOTAL_ROWS {
            if self.cache.len() <= 1 {
                break;
            }
            self.cache.remove(0);
        }
    }

    fn new_stater(&mut self, listing: Arc<Listing>) {
        let hub = self.hub.clone();
        self.stater_gen += 1;
        let generation = self.stater_gen;
        self.stater = Stater::new(listing, move || hub.send(Msg::Stats(generation)));
        self.stats_range = (0, 0);
    }

    /// Puts the cursor on the entry named when we arrived, if any.
    fn place_pending_cursor(&mut self, select: bool, settled: bool) {
        let Some(name) = self.pending_cursor.take() else { return };
        let Some(l) = self.listing.clone() else { return };
        if let Some(row) = self.rows.iter().position(|&i| *l.key(i) == *name.as_slice()) {
            self.cursor = row;
            self.anchor = row;
            self.cursor_shown = true;
            if select && self.sel_count == 0 {
                self.set_selected(self.rows[row], true);
            }
            self.ensure_visible();
        } else {
            let last = self.rows.len().saturating_sub(1);
            self.cursor = self.cursor.min(last);
            self.anchor = self.anchor.min(last);
            // It may just not have been read yet.
            if !settled {
                self.pending_cursor = Some(name);
            }
        }
    }

    /// Recomputes rows after the sort, hidden setting or filter changed. Big folders do
    /// it on a thread, and the old rows stay up until it's done.
    pub fn rederive(&mut self, resort: bool) {
        let Some(l) = self.listing.clone() else { return };
        self.derive_cancel.store(true, Ordering::Relaxed);
        self.derive_cancel = Arc::new(AtomicBool::new(false));
        self.derive_gen += 1;
        let resort = resort || self.resort_pending;
        self.resort_pending = resort;
        if !resort && self.sorted.len() < SYNC_DERIVE_LIMIT {
            let rows = visible_rows(&l, &self.sorted, self.show_hidden, &self.filter);
            self.apply_rows(rows);
            return;
        }
        let (hub, generation, sort, hidden, filter, cancel, sorted) = (self.hub.clone(), self.derive_gen, self.sort, self.show_hidden, self.filter.clone(), self.derive_cancel.clone(), self.sorted.clone());
        let spawned = std::thread::Builder::new().name("gannet-derive".into()).spawn(move || {
            let sorted = if resort { Arc::new(sort_entries(&l, sort, &cancel)) } else { sorted };
            let rows = visible_rows(&l, &sorted, hidden, &filter);
            if !cancel.load(Ordering::Relaxed) {
                hub.send(Msg::Derived { generation, listing: l, sorted, rows, sort });
            }
        });
        if spawned.is_err() {
            self.notices.push("Couldn't start a thread to sort this folder".into());
            self.invalidate_all();
        }
    }

    fn apply_rows(&mut self, rows: Vec<u32>) {
        if rows != self.rows {
            self.rows_changing();
        }
        let at = self.rows.get(self.cursor).copied();
        self.rows = rows;
        self.cursor = at.and_then(|id| self.rows.iter().position(|&i| i == id)).unwrap_or(0);
        self.anchor = self.cursor;
        self.recount_selection();
        self.clamp_scroll();
        if self.cursor_shown {
            self.ensure_visible();
        }
        self.stats_range = (0, 0);
        self.request_stats();
        self.invalidate_all();
    }

    pub fn on_msg(&mut self, m: Msg) {
        self.handle_msg(m);
        self.preview_sync();
        // A reload can move the rows under a drag, and a disk can have just answered.
        self.refresh_file_drag();
        self.refresh_incoming();
    }

    fn handle_msg(&mut self, m: Msg) {
        match m {
            Msg::Preview(e) => self.on_preview(e),
            Msg::Dropped(d) => self.on_dropped(d),
            Msg::DragSource { drag, facts } => {
                if let Some(FileDrag::Active(d)) = &mut self.file_drag
                    && d.id == drag
                {
                    d.source = facts;
                }
            }
            Msg::DragDevice { drag, path, device } => {
                if let Some(FileDrag::Active(d)) = &mut self.file_drag
                    && d.id == drag
                {
                    d.devices.insert(path, device);
                } else if let Some(i) = &mut self.incoming
                    && i.id == drag
                {
                    i.devices.insert(path, device);
                    self.refresh_incoming();
                }
            }
            Msg::Load(LoadEvent::Ready(l)) => self.on_loaded(l),
            Msg::Load(LoadEvent::Failed { generation, error, dir }) => {
                if generation == self.load_gen {
                    // `gannet some/file`: show its folder with the file under the cursor.
                    if std::mem::take(&mut self.first_load) && error == "Not a directory" {
                        use std::os::unix::ffi::OsStrExt;
                        if let (Some(parent), Some(name)) = (dir.parent(), dir.file_name()) {
                            return self.go(Stop { dir: parent.to_path_buf(), cursor_name: Some(name.as_bytes().to_vec()) });
                        }
                    }
                    self.reload_after_load = false;
                    self.hold_paint = None;
                    self.loading = false;
                    self.load_error = Some(error);
                    self.rows_changing();
                    self.listing = None;
                    self.rows.clear();
                    self.selected.clear();
                    self.sel_count = 0;
                    self.invalidate_all();
                }
            }
            Msg::Stats(g) => {
                if g == self.stater_gen {
                    self.invalidate(self.layout().list);
                    self.invalidate(self.layout().status);
                }
            }
            Msg::Derived { generation, listing, sorted, rows, sort } => {
                let current = self.listing.as_ref().is_some_and(|l| Arc::ptr_eq(l, &listing));
                if generation == self.derive_gen && current {
                    self.resort_pending = false;
                    self.sorted = sorted;
                    self.remember(listing, sort);
                    self.apply_rows(rows);
                }
            }
            Msg::Font(path, index) => {
                if self.text.add_font(&path, index) {
                    self.invalidate_all();
                }
            }
            Msg::PrimaryFont(path, index) => {
                if self.text.set_primary(&path, index) {
                    self.invalidate_all();
                }
            }
            Msg::DesktopDark(dark) => {
                if self.theme_source != Source::AliasOs {
                    self.set_colors(gannet_core::theme::builtin(dark), Source::Desktop);
                }
            }
            Msg::Notice(text) => self.notice(text),
            Msg::Op(e) => self.on_op(e),
            Msg::Clipboard(data, mime, Paste::Files { dest }) => {
                self.paste_in_flight = false;
                match data {
                    Some(data) => self.paste_files(&mime, &data, dest),
                    None => self.notice("The clipboard holds too much to paste".into()),
                }
            }
            Msg::Clipboard(data, _, Paste::IntoField { edit }) => {
                let text = String::from_utf8_lossy(data.as_deref().unwrap_or_default()).into_owned();
                let line = text.lines().next().unwrap_or("").to_string();
                if edit != self.edit_gen {
                    return;
                }
                if let Some(f) = self.field_mut() {
                    f.insert(&line);
                    self.apply_filter_field();
                    self.invalidate_all();
                }
            }
        }
    }

    pub fn notice(&mut self, text: String) {
        self.notices.push(text);
        self.clamp_scroll();
        self.invalidate_all();
    }

    fn field_mut(&mut self) -> Option<&mut Field> {
        match &mut self.edit {
            Edit::Path(f) | Edit::Filter(f) | Edit::NewFolder(f) | Edit::Rename { field: f, .. } => Some(f),
            Edit::None => None,
        }
    }

    pub fn set_colors(&mut self, c: Colors, source: Source) {
        self.theme_source = source;
        if c != self.colors {
            self.colors = c;
            self.text.set_contrast(c.dark);
            self.invalidate_all();
        }
    }

    /// Asks for sizes and times of the rows on screen, and a screenful around them.
    pub fn request_stats(&mut self) {
        let (Some(l), Some(st)) = (self.listing.clone(), self.stater.as_ref()) else { return };
        let row_h = self.layout().row_h.max(1) as f64;
        let cap = self.row_capacity();
        let first = (self.scroll / row_h) as usize;
        let range = (first.saturating_sub(cap / 2).min(self.rows.len()), (first + cap + cap / 2).min(self.rows.len()));
        if range == self.stats_range {
            return;
        }
        self.stats_range = range;
        let ids: Vec<u32> = self.rows[range.0..range.1].iter().copied().filter(|&i| l.stat(i) == Stat::Unknown).collect();
        if !ids.is_empty() {
            st.request(ids);
        }
    }

    // ---- Selection and scrolling ----

    pub fn is_selected(&self, id: u32) -> bool {
        self.selected.get(id as usize / 64).is_some_and(|w| w >> (id % 64) & 1 == 1)
    }

    fn set_selected(&mut self, id: u32, on: bool) {
        let Some(w) = self.selected.get_mut(id as usize / 64) else { return };
        let bit = 1u64 << (id % 64);
        if on && *w & bit == 0 {
            *w |= bit;
            self.sel_count += 1;
        } else if !on && *w & bit != 0 {
            *w &= !bit;
            self.sel_count -= 1;
        }
    }

    pub fn clear_selection(&mut self) {
        self.selected.iter_mut().for_each(|w| *w = 0);
        self.sel_count = 0;
    }

    /// Only rows on show count: a hidden or filtered-out entry can't be acted on by accident.
    fn recount_selection(&mut self) {
        let mut keep = vec![0u64; self.selected.len()];
        let mut n = 0;
        for &i in &self.rows {
            if self.is_selected(i) {
                keep[i as usize / 64] |= 1 << (i % 64);
                n += 1;
            }
        }
        self.selected = keep;
        self.sel_count = n;
    }

    pub fn select_all(&mut self) {
        self.selection_epoch += 1;
        for i in self.rows.clone() {
            self.set_selected(i, true);
        }
        self.invalidate_list();
    }

    fn invalidate_list(&mut self) {
        let l = self.layout();
        self.invalidate(l.list);
        self.invalidate(l.status);
    }

    /// The selected entries, in view order.
    pub fn selected_ids(&self) -> Vec<u32> {
        if self.sel_count == 0 {
            return Vec::new();
        }
        self.rows.iter().copied().filter(|&i| self.is_selected(i)).collect()
    }

    fn content_height(&self) -> f64 {
        self.rows.len() as f64 * self.layout().row_h as f64
    }

    pub fn max_scroll(&self) -> f64 {
        (self.content_height() - self.layout().list.h as f64).max(0.0)
    }

    pub fn clamp_scroll(&mut self) {
        self.scroll = self.scroll.clamp(0.0, self.max_scroll());
    }

    pub fn scroll_to(&mut self, y: f64) {
        let old = self.scroll;
        self.scroll = y;
        self.clamp_scroll();
        if self.scroll != old {
            self.request_stats();
            self.invalidate(self.layout().list);
            // What's under the pointer just changed.
            if matches!(&self.file_drag, Some(FileDrag::Band(b)) if b.active) {
                self.band_update();
            } else if self.pointer.0 >= 0 {
                let h = self.hit(self.pointer.0, self.pointer.1);
                self.set_hover(h);
            }
        }
    }

    fn ensure_visible(&mut self) {
        let l = self.layout();
        let top = self.cursor as f64 * l.row_h as f64;
        if top < self.scroll {
            self.scroll_to(top);
        } else if top + l.row_h as f64 > self.scroll + l.list.h as f64 {
            self.scroll_to(top + l.row_h as f64 - l.list.h as f64);
        }
    }

    fn page_rows(&self) -> usize {
        let l = self.layout();
        (l.list.h / l.row_h.max(1)).max(1) as usize
    }

    /// Moves the cursor. Plain moves select the row they land on, as in most file managers.
    fn move_cursor(&mut self, to: usize, m: Mods) {
        if self.rows.is_empty() {
            return;
        }
        let to = to.min(self.rows.len() - 1);
        self.selection_epoch += 1;
        // The user has moved on from wherever an arrival wanted the cursor.
        self.pending_cursor = None;
        self.anchor = self.anchor.min(self.rows.len() - 1);
        if m.shift {
            self.clear_selection();
            let (a, b) = (self.anchor.min(to), self.anchor.max(to));
            for row in a..=b {
                self.set_selected(self.rows[row], true);
            }
        } else if !m.ctrl {
            self.clear_selection();
            self.set_selected(self.rows[to], true);
            self.anchor = to;
        }
        self.cursor = to;
        self.cursor_shown = true;
        self.ensure_visible();
        self.invalidate_list();
    }

    /// A cursor that isn't showing yet appears where it is before it moves.
    pub(crate) fn step(&mut self, delta: isize, m: Mods) {
        if !self.cursor_shown {
            self.move_cursor(self.cursor, m);
            return;
        }
        self.move_cursor(self.cursor.saturating_add_signed(delta), m);
    }

    // ---- Doing things ----

    pub fn action(&mut self, a: Action) {
        match a {
            Action::Open => self.open_selection(),
            Action::Preview => self.open_preview(),
            Action::ToggleHidden => {
                self.show_hidden = !self.show_hidden;
                self.rederive(false);
                self.invalidate_all();
            }
            Action::SortBy(key) => {
                self.sort = if self.sort.key == key { SortSpec { key, descending: !self.sort.descending } } else { SortSpec { key, descending: false } };
                self.rederive(true);
                self.invalidate(self.layout().header);
            }
            Action::Reload => self.reload(),
            Action::NewWindow => {
                let hub = self.hub.clone();
                if let Ok(me) = std::env::current_exe() {
                    gannet_core::open::spawn_detached(std::process::Command::new(me).arg(&self.dir), move |e| hub.send(Msg::Notice(format!("Couldn't open a new window: {e}"))));
                }
            }
            Action::Cut if self.in_trash() => self.trash_is_read_only(),
            Action::Cut => self.put_on_clipboard(true),
            Action::Copy => self.put_on_clipboard(false),
            Action::Paste => {
                // A second press before the first answer would start the same move twice.
                if self.in_trash() {
                    self.trash_is_read_only();
                } else if !self.paste_in_flight {
                    self.paste_in_flight = true;
                    self.cmds.push(Cmd::ReadClipboard(Paste::Files { dest: self.dir.clone() }));
                }
            }
            Action::Rename | Action::NewFolder if self.in_trash() => self.trash_is_read_only(),
            Action::Rename => self.start_rename(),
            Action::Trash => self.trash_selection(),
            Action::DeleteForGood => self.ask_delete(),
            Action::NewFolder => self.start_new_folder(),
            Action::Undo => {
                if let Some(u) = self.undo.pop() {
                    self.start_job(Request::Undo(u));
                }
            }
            Action::Restore => self.restore_selection(),
            Action::EmptyTrash => self.ask_empty_trash(),
        }
    }

    /// Selects these entries of the folder showing, once it's read, with the cursor on the
    /// first. Hidden ones show, as they were asked for by name.
    pub fn select_on_arrival(&mut self, names: Vec<Vec<u8>>) {
        if names.iter().any(|n| n.first() == Some(&b'.')) {
            self.show_hidden = true;
        }
        self.pending_select = names;
    }

    /// The Trash itself, listing every Trash folder, rather than a folder inside one.
    pub fn is_trash_view(&self) -> bool {
        self.dir == self.trash_dir
    }

    fn home_trash(&self) -> PathBuf {
        self.trash_dir.parent().map_or_else(|| self.trash_dir.clone(), Path::to_path_buf)
    }

    fn restore_selection(&mut self) {
        let Some(l) = self.listing.clone().filter(|l| l.is_trash()) else { return };
        let mut items = Vec::new();
        for id in self.selected_ids() {
            match l.trashed(id) {
                Some(t) => items.push(t),
                None => self.notices.push(format!("“{}” can't be restored: the Trash has no record of where it came from", String::from_utf8_lossy(l.name(id)))),
            }
        }
        if !items.is_empty() {
            self.start_job(Request::Restore { items });
        }
        self.invalidate_all();
    }

    /// Emptying needs the whole list: while other disks are still being read, it waits.
    pub fn can_empty_trash(&self) -> bool {
        !self.loading && self.listing.as_ref().is_some_and(|l| l.is_trash() && !l.is_empty())
    }

    fn ask_empty_trash(&mut self) {
        if !self.can_empty_trash() {
            return;
        }
        let Some(l) = self.listing.clone() else { return };
        // Hidden items go too, as they would in any other file manager.
        let paths: Vec<PathBuf> = (0..l.len() as u32).map(|i| l.path(i)).collect();
        if !paths.is_empty() {
            self.dialog = Some(Dialog::ConfirmDelete { paths, at: 1, empty: true });
            self.invalidate_all();
        }
    }

    fn open_selection(&mut self) {
        let Some(l) = self.listing.clone() else { return };
        let mut ids = self.selected_ids();
        if ids.is_empty() && self.cursor_shown {
            ids.extend(self.rows.get(self.cursor));
        }
        if ids.is_empty() {
            return;
        }
        // Opening a pile of files by accident is hard to undo.
        const MANY: usize = 20;
        if ids.len() > MANY {
            self.notices.push(format!("Select {MANY} items or fewer to open them at once"));
            self.invalidate_all();
            return;
        }
        let first_dir = ids.iter().copied().find(|&i| l.kind(i).is_dirlike());
        for &i in ids.iter().filter(|&&i| !l.kind(i).is_dirlike()) {
            let hub = self.hub.clone();
            gannet_core::open::open_with_default(&l.path(i), move |p, why| {
                hub.send(Msg::Notice(format!("Couldn't open {}: {why}", p.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned()))));
            });
        }
        if let Some(i) = first_dir {
            self.navigate(l.path(i), None);
        }
    }

    pub fn go_back(&mut self) {
        let here = self.current_stop();
        if let Some(s) = self.history.go_back(here) {
            self.go(s);
        }
    }

    pub fn go_forward(&mut self) {
        let here = self.current_stop();
        if let Some(s) = self.history.go_forward(here) {
            self.go(s);
        }
    }

    pub fn go_up(&mut self) {
        if let Some(s) = self.up_target() {
            self.navigate(s.dir, s.cursor_name);
        }
    }

    /// Where Up goes. The Trash's own folders are kept out of the way: the Trash view has
    /// nothing above it, and a folder at the top of any Trash goes up to the Trash view.
    pub fn up_target(&self) -> Option<Stop> {
        if self.is_trash_view() {
            return None;
        }
        let parent = self.dir.parent()?;
        if self.trash_files_root(parent).is_some_and(|f| f == parent) {
            // The Trash view finds rows by their real path, which is where we are.
            return Some(Stop { dir: self.trash_dir.clone(), cursor_name: Some(self.dir.clone().into_os_string().into_vec()) });
        }
        parent_stop(&self.dir)
    }

    /// The `files` folder of whichever of this user's Trash folders holds `path`, if any.
    pub fn trash_files_root(&self, path: &Path) -> Option<PathBuf> {
        let (home, uid) = (self.home_trash(), rustix::process::geteuid().as_raw());
        path.ancestors().filter(|a| a.file_name().is_some_and(|n| n == "files")).find(|a| trash::is_inside_trash(&home, uid, a) && !a.parent().is_some_and(|g| trash::is_inside_trash(&home, uid, g))).map(Path::to_path_buf)
    }

    /// The toolbar's Empty Trash button, shown in the Trash view.
    pub fn empty_trash_rect(&mut self) -> Option<Rect> {
        if !self.listing.as_ref().is_some_and(|l| l.is_trash()) {
            return None;
        }
        let l = self.layout();
        let end = self.filter_rect().map_or(l.toolbar.right() - self.px(8.0), |r| r.x - self.px(8.0));
        let w = self.text.measure("Empty Trash") as i32 + self.px(24.0);
        Some(Rect::new(end - w, self.px(7.0), w, l.toolbar.h - self.px(14.0)))
    }

    fn set_edit(&mut self, e: Edit) {
        self.edit = e;
        self.invalidate(self.layout().toolbar);
    }

    pub fn start_path_edit(&mut self) {
        let mut f = Field::new(&self.dir.to_string_lossy());
        f.select_all();
        self.set_edit(Edit::Path(f));
    }

    fn start_filter(&mut self, initial: &str) {
        let mut f = Field::new(&self.filter);
        f.insert(initial);
        self.set_edit(Edit::Filter(f));
        self.apply_filter_field();
    }

    fn apply_filter_field(&mut self) {
        if let Edit::Filter(f) = &self.edit {
            let text = f.text.clone();
            if text != self.filter {
                self.filter = text;
                self.rederive(false);
            }
        }
        self.invalidate(self.layout().toolbar);
        self.invalidate(self.layout().status);
    }

    fn commit_path(&mut self, text: &str) {
        let expanded = expand_home(text, self.places.first().map(|p| p.path.as_path()));
        self.set_edit(Edit::None);
        if !expanded.is_absolute() {
            self.notices.push("Enter a full path, starting with / or ~".to_string());
            self.invalidate_all();
            return;
        }
        self.navigate(clean(&expanded), None);
    }

    fn type_ahead(&mut self, s: &str) {
        if self.type_ahead_at.elapsed() > TYPE_AHEAD_RESET {
            self.type_ahead.clear();
        }
        self.type_ahead_at = Instant::now();
        self.type_ahead.push_str(&s.to_lowercase());
        let Some(l) = self.listing.clone() else { return };
        let needle = self.type_ahead.clone();
        let n = self.rows.len();
        if n == 0 {
            return;
        }
        // Pressing one letter over and over walks through the names starting with it.
        let mut letters = needle.chars();
        let first = letters.next();
        let repeating = needle.chars().count() > 1 && letters.all(|c| Some(c) == first);
        let needle = if repeating { first.map_or(String::new(), String::from) } else { needle };
        // Otherwise the current row stays if it still matches. A first key looks past it,
        // unless no cursor is showing yet, when the top row counts too.
        let single = needle.chars().count() == 1;
        let start = if single && self.cursor_shown { self.cursor + 1 } else { self.cursor };
        let matches = |row: usize| String::from_utf8_lossy(l.name(self.rows[row])).to_lowercase().starts_with(&needle);
        if let Some(found) = (0..n).map(|d| (start + d) % n).find(|&r| matches(r)) {
            self.move_cursor(found, Mods::default());
        }
    }

    fn selected_paths(&self) -> Vec<PathBuf> {
        let Some(l) = &self.listing else { return Vec::new() };
        self.selected_ids().into_iter().map(|i| l.path(i)).collect()
    }

    /// The Trash's `files` and `info` folders have to stay paired, so items can only
    /// leave as copies, or be deleted for good.
    fn trash_is_read_only(&mut self) {
        self.notice("Items in the Trash can't be changed. Copy them out, or delete them for good".into());
    }

    fn start_job(&mut self, r: Request) {
        let ends_undo = matches!(r, Request::Copy { .. } | Request::Link { .. } | Request::Delete { .. });
        let job = self.engine.start(r);
        self.jobs.push(JobView { job, started: Instant::now(), ends_undo, selection_epoch: self.selection_epoch });
    }

    fn in_trash(&self) -> bool {
        trash::is_inside_trash(&self.home_trash(), rustix::process::geteuid().as_raw(), &self.dir)
    }

    fn put_on_clipboard(&mut self, cut: bool) {
        let paths = self.selected_paths();
        if paths.is_empty() {
            return;
        }
        self.cmds.push(Cmd::SetClipboard(clipboard::encode(&paths, cut)));
        self.cut_paths = if cut { paths } else { Vec::new() };
        self.invalidate_list();
    }

    fn paste_files(&mut self, mime: &str, data: &[u8], dest: PathBuf) {
        let (paths, cut) = clipboard::parse(mime, data);
        if paths.is_empty() {
            self.notice("There are no files on the clipboard to paste".into());
            return;
        }
        if cut {
            // It only moves once: a second paste would look for files no longer there.
            self.cut_paths.clear();
            self.cmds.push(Cmd::ClearClipboard);
            self.invalidate_list();
            self.start_job(Request::Move { sources: paths, dest });
        } else {
            self.start_job(Request::Copy { sources: paths, dest });
        }
    }

    fn start_rename(&mut self) {
        let Some(l) = self.listing.clone() else { return };
        let ids = self.selected_ids();
        let [id] = ids.as_slice() else {
            if ids.len() > 1 {
                self.notice("Select one item to rename it".into());
            }
            return;
        };
        let name = l.name(*id).to_vec();
        let mut field = Field::new(&String::from_utf8_lossy(&name));
        if l.kind(*id).is_dirlike() {
            field.select_all();
        } else {
            field.select_stem();
        }
        if let Some(row) = self.rows.iter().position(|i| i == id) {
            self.cursor = row;
            self.ensure_visible();
        }
        self.edit_gen += 1;
        self.edit = Edit::Rename { initial: field.text.clone(), name, path: l.path(*id), field };
        self.invalidate_all();
    }

    fn start_new_folder(&mut self) {
        let mut f = Field::new("New Folder");
        f.select_all();
        self.scroll_to(0.0);
        self.edit_gen += 1;
        self.edit = Edit::NewFolder(f);
        self.invalidate_all();
    }

    fn trash_selection(&mut self) {
        let paths = self.selected_paths();
        if paths.is_empty() {
            return;
        }
        // Items already in the Trash have nowhere further to go.
        if self.in_trash() {
            self.dialog = Some(Dialog::ConfirmDelete { paths, at: 1, empty: false });
            self.invalidate_all();
            return;
        }
        self.start_job(Request::Trash { paths });
    }

    fn ask_delete(&mut self) {
        let paths = self.selected_paths();
        if !paths.is_empty() {
            // Cancel has the focus: Enter shouldn't destroy anything by reflex.
            self.dialog = Some(Dialog::ConfirmDelete { paths, at: 1, empty: false });
            self.invalidate_all();
        }
    }

    fn on_op(&mut self, e: ops::Event) {
        match e {
            ops::Event::Progress(_) => self.invalidate(self.layout().notices),
            ops::Event::Conflict(c) => {
                if self.dialog.is_none() {
                    self.show_conflict(c);
                    self.invalidate_all();
                } else {
                    self.pending_conflicts.push(c);
                }
            }
            ops::Event::Error { path, message, .. } => {
                let name = path.file_name().map_or_else(|| path.to_string_lossy().into_owned(), |n| n.to_string_lossy().into_owned());
                self.notice(format!("{name}: {message}"));
            }
            ops::Event::Finished(o) => {
                self.answer_drop(&o);
                let view = self.jobs.iter().position(|j| j.job.id == o.job).map(|i| self.jobs.remove(i));
                let selection_untouched = view.as_ref().is_some_and(|v| v.selection_epoch == self.selection_epoch);
                if o.cancelled
                    && let Some(v) = &view
                {
                    self.notice(format!("Stopped: {}", v.job.label));
                }
                match o.undo {
                    Some(u) => {
                        self.undo.push(u);
                        if self.undo.len() > UNDO_DEPTH {
                            self.undo.remove(0);
                        }
                    }
                    // Copying or deleting can't be undone, and undoing something older
                    // right after would be a surprise.
                    None if view.is_some_and(|v| v.ends_undo) => self.undo.clear(),
                    None => {}
                }
                // Another disk's Trash may not be watched, and the Trash view shows it too.
                let trash_touched = self.is_trash_view() && o.touched.iter().any(|t| self.trash_files_root(t).is_some());
                if o.touched.contains(&self.dir) || trash_touched {
                    use std::os::unix::ffi::OsStrExt;
                    // A selection made since the job began is the user's, and stays.
                    self.pending_select = o.created.iter().filter(|_| selection_untouched).filter(|p| p.parent() == Some(self.dir.as_path())).filter_map(|p| p.file_name()).map(|n| n.as_bytes().to_vec()).collect();
                    self.reload_at = None;
                    self.reload();
                }
                self.clamp_scroll();
                self.invalidate_all();
                if self.quit_when_idle && self.jobs.is_empty() {
                    self.cmds.push(Cmd::Quit);
                }
            }
        }
    }

    pub fn dialog_buttons(d: &Dialog) -> Vec<DialogButton> {
        let b = |label, key, danger| DialogButton { label, key, danger };
        match d {
            Dialog::Conflict { conflict, .. } => {
                let mut v = Vec::new();
                if conflict.can_replace {
                    // No letter: this one overwrites, so it takes a deliberate choice.
                    v.push(b(if conflict.both_folders { "Merge" } else { "Replace" }, "", !conflict.both_folders));
                }
                v.push(b("Skip", "S", false));
                v.push(b("Keep both", "K", false));
                v.push(b("Cancel", "Esc", false));
                v
            }
            Dialog::ConfirmDelete { empty: true, .. } => vec![b("Empty Trash", "E", true), b("Cancel", "Esc", false)],
            Dialog::ConfirmDelete { .. } => vec![b("Delete", "D", true), b("Cancel", "Esc", false)],
            Dialog::ConfirmQuit { .. } => vec![b("Stop them and close", "S", true), b("Keep working", "Esc", false)],
        }
    }

    /// Conflicts appear on their own, so Enter means Skip and nothing is answered in the
    /// moment after one pops up.
    fn show_conflict(&mut self, conflict: Conflict) {
        let at = usize::from(conflict.can_replace);
        self.dialog = Some(Dialog::Conflict { conflict, apply_to_all: false, at });
        self.dialog_guard = Some(Instant::now());
    }

    fn dialog_guarded(&self) -> bool {
        self.dialog_guard.is_some_and(|t| t.elapsed() < DIALOG_GUARD)
    }

    fn press_dialog_button(&mut self, i: usize) {
        let Some(d) = self.dialog.take() else { return };
        let labels: Vec<&str> = Self::dialog_buttons(&d).iter().map(|b| b.label).collect();
        let Some(&label) = labels.get(i) else {
            self.dialog = Some(d);
            return;
        };
        match d {
            Dialog::Conflict { conflict, apply_to_all, .. } => {
                let choice = match label {
                    "Replace" | "Merge" => Choice::Replace,
                    "Skip" => Choice::Skip,
                    "Keep both" => Choice::KeepBoth,
                    _ => Choice::Cancel,
                };
                let _ = conflict.reply.send(Answer { choice, apply_to_all });
            }
            Dialog::ConfirmDelete { paths, .. } => {
                if label == "Delete" || label == "Empty Trash" {
                    self.start_job(Request::Delete { paths });
                }
            }
            Dialog::ConfirmQuit { .. } => {
                if label != "Keep working" {
                    self.jobs.iter().for_each(|j| j.job.cancel());
                    self.quit_when_idle = true;
                    if self.jobs.is_empty() {
                        self.cmds.push(Cmd::Quit);
                    }
                }
            }
        }
        // Any other job's question is next.
        if !self.pending_conflicts.is_empty() {
            let c = self.pending_conflicts.remove(0);
            self.show_conflict(c);
        }
        self.invalidate_all();
    }

    fn dialog_key(&mut self, sym: u32, utf8: &str) {
        if self.dialog_guarded() {
            return;
        }
        let Some(d) = self.dialog.as_mut() else { return };
        let buttons = Self::dialog_buttons(d);
        let n = buttons.len();
        let at = match d {
            Dialog::Conflict { at, .. } | Dialog::ConfirmDelete { at, .. } | Dialog::ConfirmQuit { at } => at,
        };
        match sym {
            k::KEY_Left => *at = (*at + n - 1) % n,
            k::KEY_Right | k::KEY_Tab => *at = (*at + 1) % n,
            k::KEY_Return | k::KEY_KP_Enter => {
                let i = *at;
                return self.press_dialog_button(i);
            }
            k::KEY_Escape => return self.press_dialog_button(n - 1),
            _ => {
                if let Dialog::Conflict { apply_to_all, .. } = d
                    && utf8.eq_ignore_ascii_case("a")
                {
                    *apply_to_all = !*apply_to_all;
                } else if let Some(i) = buttons.iter().position(|b| !utf8.is_empty() && b.key.eq_ignore_ascii_case(utf8)) {
                    return self.press_dialog_button(i);
                }
            }
        }
        self.invalidate_all();
    }

    pub fn dialog_rect(&self) -> Rect {
        let (w, h) = (self.px(520.0).min(self.w), self.px(170.0).min(self.h));
        Rect::new((self.w - w) / 2, (self.h - h) / 2, w, h)
    }

    /// The button rectangles, right-aligned along the dialog's bottom.
    pub fn dialog_button_rects(&mut self, d: &Dialog) -> Vec<Rect> {
        let r = self.dialog_rect();
        let buttons = Self::dialog_buttons(d);
        let h = self.px(30.0);
        let y = r.bottom() - h - self.px(12.0);
        let mut x = r.right() - self.px(12.0);
        let mut out = vec![Rect::default(); buttons.len()];
        for (i, b) in buttons.iter().enumerate().rev() {
            let w = self.text.measure(b.label) as i32 + self.px(24.0);
            x -= w;
            out[i] = Rect::new(x, y, w, h);
            x -= self.px(8.0);
        }
        out
    }

    pub fn dialog_check_rect(&self) -> Rect {
        let r = self.dialog_rect();
        Rect::new(r.x + self.px(16.0), r.bottom() - self.px(76.0), r.w - self.px(32.0), self.px(24.0))
    }

    /// Closing the window would stop what's running, so that gets asked first.
    pub(crate) fn request_close(&mut self) {
        if self.jobs.is_empty() {
            self.cmds.push(Cmd::Quit);
        } else if self.dialog.is_none() {
            self.dialog = Some(Dialog::ConfirmQuit { at: 1 });
            self.invalidate_all();
        }
    }

    fn context_items(&self) -> Vec<MenuItem> {
        let any = self.sel_count > 0;
        let one = self.sel_count == 1;
        if self.listing.as_ref().is_some_and(|l| l.is_trash()) {
            return vec![
                MenuItem { label: "Open", keys: "Enter", action: Action::Open, enabled: any },
                MenuItem { label: "Preview", keys: "Space", action: Action::Preview, enabled: any },
                MenuItem { label: "Copy", keys: "Ctrl+C", action: Action::Copy, enabled: any },
                MenuItem { label: "Restore", keys: "", action: Action::Restore, enabled: any },
                MenuItem { label: "Delete for good", keys: "Delete", action: Action::DeleteForGood, enabled: any },
                MenuItem { label: "Empty Trash", keys: "", action: Action::EmptyTrash, enabled: self.can_empty_trash() },
                MenuItem { label: if self.show_hidden { "Hide hidden files" } else { "Show hidden files" }, keys: "Ctrl+H", action: Action::ToggleHidden, enabled: true },
                MenuItem { label: "Sort by name", keys: "Ctrl+1", action: Action::SortBy(SortKey::Name), enabled: true },
                MenuItem { label: "Sort by size", keys: "Ctrl+2", action: Action::SortBy(SortKey::Size), enabled: true },
                MenuItem { label: "Sort by date deleted", keys: "Ctrl+3", action: Action::SortBy(SortKey::Modified), enabled: true },
                MenuItem { label: "Reload", keys: "F5", action: Action::Reload, enabled: true },
                MenuItem { label: "New window", keys: "Ctrl+N", action: Action::NewWindow, enabled: true },
            ];
        }
        vec![
            MenuItem { label: "Open", keys: "Enter", action: Action::Open, enabled: any },
            MenuItem { label: "Preview", keys: "Space", action: Action::Preview, enabled: any },
            MenuItem { label: "Cut", keys: "Ctrl+X", action: Action::Cut, enabled: any },
            MenuItem { label: "Copy", keys: "Ctrl+C", action: Action::Copy, enabled: any },
            MenuItem { label: "Paste", keys: "Ctrl+V", action: Action::Paste, enabled: true },
            MenuItem { label: "Rename", keys: "F2", action: Action::Rename, enabled: one },
            MenuItem { label: "Move to Trash", keys: "Delete", action: Action::Trash, enabled: any },
            MenuItem { label: "Delete for good", keys: "Shift+Delete", action: Action::DeleteForGood, enabled: any },
            MenuItem { label: "New folder", keys: "Ctrl+Shift+N", action: Action::NewFolder, enabled: true },
            MenuItem { label: "Undo", keys: "Ctrl+Z", action: Action::Undo, enabled: !self.undo.is_empty() },
            MenuItem { label: if self.show_hidden { "Hide hidden files" } else { "Show hidden files" }, keys: "Ctrl+H", action: Action::ToggleHidden, enabled: true },
            MenuItem { label: "Sort by name", keys: "Ctrl+1", action: Action::SortBy(SortKey::Name), enabled: true },
            MenuItem { label: "Sort by size", keys: "Ctrl+2", action: Action::SortBy(SortKey::Size), enabled: true },
            MenuItem { label: "Sort by modified", keys: "Ctrl+3", action: Action::SortBy(SortKey::Modified), enabled: true },
            MenuItem { label: "Reload", keys: "F5", action: Action::Reload, enabled: true },
            MenuItem { label: "New window", keys: "Ctrl+N", action: Action::NewWindow, enabled: true },
        ]
    }

    fn open_menu(&mut self, x: i32, y: i32) {
        self.menu = Some(Menu { items: self.context_items(), x, y, at: usize::MAX });
        self.invalidate_all();
    }

    fn close_menu(&mut self) {
        self.menu = None;
        self.invalidate_all();
    }

    pub fn menu_rect(&self, m: &Menu) -> Rect {
        let (w, row) = (self.px(250.0), self.px(28.0));
        let h = row * m.items.len() as i32 + self.px(8.0);
        Rect::new(m.x.min(self.w - w).max(0), m.y.min(self.h - h).max(0), w, h)
    }

    // ---- Input ----

    pub fn input(&mut self, i: Input) {
        self.handle_input(i);
        // Whatever just happened may have opened a menu or dialog, like a close request.
        self.refresh_file_drag();
    }

    fn handle_input(&mut self, i: Input) {
        match i {
            Input::Mods(m) => {
                self.mods_now = m;
                self.refresh_file_drag();
                self.refresh_incoming();
            }
            // Another program has the clipboard now, so nothing's about to move.
            Input::ClipboardLost => {
                if !self.cut_paths.is_empty() {
                    self.cut_paths.clear();
                    self.invalidate_list();
                }
            }
            Input::Key { keysym, utf8, mods, repeat } => {
                self.key_is_repeat = repeat;
                self.key(keysym, &utf8, mods);
            }
            Input::Move { x, y } => self.pointer_moved(x, y),
            Input::Leave => {
                match &self.file_drag {
                    // The drag goes on, over whatever's outside. If the button has really been
                    // let go, the compositor refuses it and says so.
                    Some(FileDrag::Active(d)) if !d.wayland => self.start_wayland_drag(),
                    Some(FileDrag::Active(_)) => {}
                    _ => self.cancel_file_drag(),
                }
                self.set_hover(Hit::None);
            }
            Input::Button { button, pressed } => self.button(button, pressed),
            Input::Scroll { lines, pixels } => {
                if self.menu.is_none() && self.preview.is_none() {
                    let row = self.layout().row_h as f64;
                    self.scroll_to(self.scroll + lines * 3.0 * row + pixels * self.scale);
                    self.refresh_file_drag();
                }
            }
            Input::Close => self.request_close(),
            Input::Focus(true) => {
                self.has_keyboard = true;
                self.ask_desktop_theme();
            }
            // The release can go to whichever window has the pointer now. A Wayland drag is the
            // compositor's, and many compositors take the keyboard away during one.
            Input::Focus(false) => {
                self.has_keyboard = false;
                // The release of a key held now won't reach this window.
                self.mods_now = Mods::default();
                self.refresh_incoming();
                if !matches!(&self.file_drag, Some(FileDrag::Active(d)) if d.wayland) {
                    self.cancel_file_drag();
                }
            }
            Input::DragEnter { x, y, files, can_copy, can_move, marker } => self.drag_entered(x, y, files, can_copy, can_move, marker),
            Input::DragMove { x, y } => {
                self.set_pointer(x, y);
                if let Some(FileDrag::Active(d)) = &mut self.file_drag
                    && d.wayland
                {
                    d.moved_inside = true;
                }
                self.refresh_file_drag();
                if let Some(i) = &mut self.incoming {
                    i.moved = true;
                }
                self.refresh_incoming();
            }
            Input::DragLeave => self.drag_left(),
            Input::DragDrop => self.drag_dropped(),
            // If it's over this window, the drop is landing here and is about to arrive.
            Input::OutDragDropped => {
                if matches!(&self.file_drag, Some(FileDrag::Active(d)) if d.wayland && !d.inside)
                    && let Some(FileDrag::Active(d)) = self.file_drag.take()
                {
                    self.end_active_drag(&d);
                }
            }
            Input::DragDropBusy => {
                self.cancel_file_drag();
                if let Some(i) = self.incoming.take() {
                    self.end_incoming(&i);
                }
                self.notice("Another drop is still being dealt with, so nothing moved".into());
            }
            Input::OutDragEnded => {
                if matches!(&self.file_drag, Some(FileDrag::Active(d)) if d.wayland)
                    && let Some(FileDrag::Active(d)) = self.file_drag.take()
                {
                    self.end_active_drag(&d);
                }
            }
            Input::Resized => {}
        }
    }

    /// Non-aliasOS desktops: `gsettings` may know better than the GTK file. It's a process,
    /// so it runs off to the side. aliasOS themes never need it.
    pub fn ask_desktop_theme(&self) {
        if self.theme_source == Source::AliasOs {
            return;
        }
        let hub = self.hub.clone();
        let _ = std::thread::Builder::new().name("gannet-gsettings".into()).spawn(move || {
            if let Some(dark) = gannet_core::theme::query_gsettings_dark() {
                hub.send(Msg::DesktopDark(dark));
            }
        });
    }

    fn key(&mut self, sym: u32, utf8: &str, m: Mods) {
        if matches!(self.file_drag, Some(FileDrag::Band(_))) {
            if sym == k::KEY_Escape {
                self.band_put_back();
            }
            return;
        }
        if matches!(self.file_drag, Some(FileDrag::Active(_))) {
            if sym == k::KEY_Escape {
                self.cancel_file_drag();
            }
            return;
        }
        if self.dialog.is_some() {
            self.dialog_key(sym, utf8);
        } else if self.menu.is_some() {
            self.menu_key(sym);
        } else if self.preview.is_some() {
            self.preview_key(sym, m);
        } else if !matches!(self.edit, Edit::None) {
            self.field_key(sym, utf8, m);
        } else {
            self.list_key(sym, utf8, m);
        }
    }

    fn menu_key(&mut self, sym: u32) {
        let Some(menu) = self.menu.as_mut() else { return };
        let n = menu.items.len();
        match sym {
            k::KEY_Escape | k::KEY_Menu | k::KEY_F10 => return self.close_menu(),
            k::KEY_Down => menu.at = next_enabled(&menu.items, menu.at, 1),
            k::KEY_Up => menu.at = next_enabled(&menu.items, if menu.at == usize::MAX { n } else { menu.at }, n - 1),
            k::KEY_Return | k::KEY_KP_Enter => {
                if let Some(item) = menu.items.get(menu.at).filter(|i| i.enabled) {
                    let a = item.action;
                    self.close_menu();
                    self.action(a);
                }
                return;
            }
            _ => return,
        }
        self.invalidate_all();
    }

    fn field_key(&mut self, sym: u32, utf8: &str, m: Mods) {
        let is_filter = matches!(self.edit, Edit::Filter(_));
        let selected = self.field_mut().map(|f| {
            let (a, b) = f.selection();
            f.text[a..b].to_string()
        });
        if m.ctrl && matches!(sym, k::KEY_c | k::KEY_x) && selected.as_ref().is_some_and(|t| !t.is_empty()) {
            let t = selected.unwrap_or_default().into_bytes();
            self.cmds.push(Cmd::SetClipboard(vec![(clipboard::TEXT, t.clone()), (clipboard::TEXT_PLAIN, t.clone()), (clipboard::UTF8_STRING, t)]));
            if sym == k::KEY_x
                && let Some(f) = self.field_mut()
            {
                f.backspace(false);
                self.apply_filter_field();
                self.invalidate_all();
            }
            return;
        }
        if m.ctrl && sym == k::KEY_v {
            self.cmds.push(Cmd::ReadClipboard(Paste::IntoField { edit: self.edit_gen }));
            return;
        }
        let Some(f) = self.field_mut() else { return };
        match sym {
            k::KEY_Left => f.left(m.ctrl, m.shift),
            k::KEY_Right => f.right(m.ctrl, m.shift),
            k::KEY_Home => f.move_to(0, m.shift),
            k::KEY_End => f.move_to(f.text.len(), m.shift),
            k::KEY_BackSpace => f.backspace(m.ctrl),
            k::KEY_Delete => f.delete(m.ctrl),
            k::KEY_a if m.ctrl => f.select_all(),
            k::KEY_Escape => return self.field_cancel(),
            k::KEY_Return | k::KEY_KP_Enter => return self.field_commit(),
            k::KEY_Down | k::KEY_Tab if is_filter => {
                self.set_edit(Edit::None);
                if !self.cursor_shown {
                    self.step(0, Mods::default());
                }
                return;
            }
            _ => {
                if !m.ctrl && !m.alt && !m.logo && !utf8.is_empty() && !utf8.chars().any(char::is_control) {
                    f.insert(utf8);
                } else {
                    return;
                }
            }
        }
        self.apply_filter_field();
        self.invalidate_all();
    }

    fn field_cancel(&mut self) {
        let was_filter = matches!(self.edit, Edit::Filter(_));
        self.set_edit(Edit::None);
        if was_filter && !self.filter.is_empty() {
            self.filter.clear();
            self.rederive(false);
        }
        self.invalidate_all();
    }

    fn field_commit(&mut self) {
        match std::mem::replace(&mut self.edit, Edit::None) {
            Edit::Path(f) => self.commit_path(&f.text),
            Edit::Filter(_) => {
                self.invalidate(self.layout().toolbar);
                if !self.cursor_shown {
                    self.step(0, Mods::default());
                }
            }
            Edit::Rename { initial, path, field, .. } => {
                self.invalidate_all();
                // Against what the field started with: a name that isn't valid UTF-8 shows
                // differently from its bytes, and pressing Enter on it must not rewrite it.
                if field.text != initial {
                    if let Err(e) = ops::validate_name(&field.text) {
                        self.notice(e.to_string());
                    } else {
                        self.start_job(Request::Rename { from: path, to_name: field.text.into() });
                    }
                }
            }
            Edit::NewFolder(field) => {
                self.invalidate_all();
                if let Err(e) = ops::validate_name(&field.text) {
                    self.notice(e.to_string());
                } else {
                    let parent = self.dir.clone();
                    self.start_job(Request::NewFolder { parent, name: field.text.into() });
                }
            }
            Edit::None => {}
        }
    }

    fn list_key(&mut self, sym: u32, utf8: &str, m: Mods) {
        let page = self.page_rows() as isize;
        match (sym, m.ctrl, m.alt) {
            (k::KEY_Up, _, true) => self.go_up(),
            (k::KEY_Left, _, true) => self.go_back(),
            (k::KEY_Right, _, true) => self.go_forward(),
            (k::KEY_Home, _, true) => {
                if let Some(home) = self.places.first().map(|p| p.path.clone()) {
                    self.navigate(home, None);
                }
            }
            (k::KEY_Up, _, _) => self.step(-1, m),
            (k::KEY_Down, _, _) => self.step(1, m),
            (k::KEY_Page_Up, _, _) => self.step(-page, m),
            (k::KEY_Page_Down, _, _) => self.step(page, m),
            (k::KEY_Home, _, _) => self.move_cursor(0, m),
            (k::KEY_End, _, _) => self.move_cursor(usize::MAX, m),
            (k::KEY_BackSpace, _, _) => self.go_up(),
            (k::KEY_Return | k::KEY_KP_Enter, _, _) => self.action(Action::Open),
            // Holding either of these would flicker, and neither means anything repeated.
            (k::KEY_space, _, false) if self.key_is_repeat => {}
            (k::KEY_space, false, false) => self.action(Action::Preview),
            (k::KEY_space, true, false) => {
                if let Some(&id) = self.rows.get(self.cursor) {
                    self.selection_epoch += 1;
                    self.cursor_shown = true;
                    self.set_selected(id, !self.is_selected(id));
                    self.anchor = self.cursor;
                    self.invalidate_list();
                }
            }
            // Each press undoes one thing, and holding it shouldn't run through them all.
            (k::KEY_Escape, _, _) if self.key_is_repeat => {}
            (k::KEY_Escape, _, _) => {
                if !self.filter.is_empty() {
                    self.filter.clear();
                    self.rederive(false);
                    self.invalidate(self.layout().toolbar);
                } else if self.sel_count > 0 {
                    self.selection_epoch += 1;
                    self.clear_selection();
                    self.invalidate_list();
                } else if self.notices.pop().is_some() {
                    self.clamp_scroll();
                    self.invalidate_all();
                }
            }
            (k::KEY_F2, _, _) => self.action(Action::Rename),
            (k::KEY_Delete, _, _) if m.shift => self.action(Action::DeleteForGood),
            (k::KEY_Delete, _, _) => self.action(Action::Trash),
            (k::KEY_c | k::KEY_C, true, _) if m.shift => {
                let text = self.selected_paths().iter().map(|p| p.to_string_lossy().into_owned()).collect::<Vec<_>>().join("\n").into_bytes();
                if !text.is_empty() {
                    self.cmds.push(Cmd::SetClipboard(vec![(clipboard::TEXT, text.clone()), (clipboard::TEXT_PLAIN, text.clone()), (clipboard::UTF8_STRING, text)]));
                }
            }
            (k::KEY_c, true, _) => self.action(Action::Copy),
            (k::KEY_x, true, _) if !m.shift => self.action(Action::Cut),
            (k::KEY_v, true, _) if !m.shift => self.action(Action::Paste),
            (k::KEY_z, true, _) if !m.shift => self.action(Action::Undo),
            (k::KEY_N, true, _) | (k::KEY_n, true, _) if m.shift => self.action(Action::NewFolder),
            (k::KEY_period, true, _) => self.stop_newest_job(),
            (k::KEY_F5, _, _) | (k::KEY_r, true, _) => self.action(Action::Reload),
            (k::KEY_F10, _, _) | (k::KEY_Menu, _, _) => self.keyboard_menu(),
            (k::KEY_h, true, _) => self.action(Action::ToggleHidden),
            (k::KEY_l, true, _) => self.start_path_edit(),
            (k::KEY_f, true, _) => self.start_filter(""),
            (k::KEY_a, true, _) => self.select_all(),
            (k::KEY_n, true, _) if !m.shift => self.action(Action::NewWindow),
            (k::KEY_q | k::KEY_w, true, _) => self.request_close(),
            (k::KEY_1, true, _) => self.action(Action::SortBy(SortKey::Name)),
            (k::KEY_2, true, _) => self.action(Action::SortBy(SortKey::Size)),
            (k::KEY_3, true, _) => self.action(Action::SortBy(SortKey::Modified)),
            (_, false, true) => {
                if let Some(d) = utf8.chars().next().and_then(|c| c.to_digit(10)).filter(|&d| d > 0)
                    && let Some(p) = self.places.get(d as usize - 1).map(|p| p.path.clone())
                {
                    self.navigate(p, None);
                }
            }
            (_, false, false) if !m.logo && utf8 == "/" => self.start_filter(""),
            (_, false, false) if !m.logo && !utf8.is_empty() && !utf8.chars().any(char::is_control) => self.type_ahead(utf8),
            _ => {}
        }
    }

    pub(crate) fn stop_newest_job(&self) {
        if let Some(j) = self.jobs.last() {
            j.job.cancel();
        }
    }

    fn keyboard_menu(&mut self) {
        let l = self.layout();
        let row_y = (l.list.y as f64 + self.cursor as f64 * l.row_h as f64 - self.scroll + l.row_h as f64) as i32;
        if !self.cursor_shown {
            self.step(0, Mods::default());
        }
        self.open_menu(l.list.x + self.px(40.0), row_y.min(l.list.bottom()).max(l.list.y));
        if let Some(m) = self.menu.as_mut() {
            m.at = next_enabled(&m.items, usize::MAX, 1);
        }
    }

    pub fn hit(&mut self, x: i32, y: i32) -> Hit {
        if let Some(d) = self.dialog.take() {
            let rects = self.dialog_button_rects(&d);
            let check = matches!(d, Dialog::Conflict { .. }) && self.dialog_check_rect().contains(x, y);
            self.dialog = Some(d);
            if check {
                return Hit::DialogCheck;
            }
            return rects.iter().position(|r| r.contains(x, y)).map_or(Hit::Outside, Hit::DialogButton);
        }
        if self.preview.is_some() {
            return if self.preview_rect().contains(x, y) { Hit::PreviewBody } else { Hit::Outside };
        }
        if let Some(m) = &self.menu {
            let r = self.menu_rect(m);
            if r.contains(x, y) {
                let i = (y - r.y - self.px(4.0)) / self.px(28.0).max(1);
                if (i as usize) < m.items.len() && i >= 0 {
                    return Hit::MenuItem(i as usize);
                }
            }
            return Hit::Outside;
        }
        let l = self.layout();
        if l.toolbar.contains(x, y) {
            let b = self.px(36.0);
            let pad = self.px(6.0);
            for (i, hit) in [Hit::Back, Hit::Forward, Hit::Up].into_iter().enumerate() {
                if Rect::new(pad + i as i32 * b, 0, b, l.toolbar.h).contains(x, y) {
                    return hit;
                }
            }
            if self.filter_rect().is_some_and(|r| r.contains(x, y)) {
                return Hit::FilterBox;
            }
            if self.empty_trash_rect().is_some_and(|r| r.contains(x, y)) {
                // An empty Trash leaves the button greyed out, with nothing to press.
                return if self.can_empty_trash() { Hit::EmptyTrash } else { Hit::None };
            }
            if let Edit::Path(_) = self.edit {
                return Hit::PathBar;
            }
            if let Some(i) = self.crumb_at(x) {
                return Hit::Crumb(i);
            }
            return Hit::PathBar;
        }
        if l.sidebar.contains(x, y) {
            let row = self.px(30.0);
            let i = (y - l.sidebar.y - self.px(8.0)) / row.max(1);
            if i >= 0 && (i as usize) < self.places.len() {
                return Hit::Place(i as usize);
            }
            return Hit::None;
        }
        if l.header.contains(x, y) {
            let date_x = l.header.right() - self.px(14.0) - l.col_date;
            let size_x = date_x - l.col_size;
            return Hit::Header(if x >= date_x {
                SortKey::Modified
            } else if x >= size_x {
                SortKey::Size
            } else {
                SortKey::Name
            });
        }
        if l.notices.contains(x, y) {
            let i = ((y - l.notices.y) / self.px(30.0).max(1)) as usize;
            if x >= l.notices.right() - self.px(30.0) {
                let jobs = self.visible_jobs().count().min(4);
                return if i < jobs { Hit::JobCancel(i) } else { Hit::NoticeClose(i - jobs) };
            }
            return Hit::None;
        }
        if l.list.contains(x, y) {
            if self.scrollbar_visible() && x >= l.list.right() - self.px(12.0) {
                return Hit::Scrollbar;
            }
            let row = ((y - l.list.y) as f64 + self.scroll) as i64 / l.row_h.max(1) as i64;
            if (row as usize) < self.rows.len() {
                return Hit::Row(row as usize);
            }
            return Hit::ListEmpty;
        }
        Hit::None
    }

    pub fn scrollbar_visible(&self) -> bool {
        self.max_scroll() > 0.0
    }

    /// The thumb's top and height, in the list's coordinates.
    pub fn scrollbar_thumb(&self) -> (i32, i32) {
        let l = self.layout();
        let content = self.content_height().max(1.0);
        let h = ((l.list.h as f64 / content * l.list.h as f64) as i32).max(self.px(24.0)).min(l.list.h);
        let travel = (l.list.h - h) as f64;
        let max = self.max_scroll();
        let y = if max > 0.0 { (self.scroll / max * travel) as i32 } else { 0 };
        (y, h)
    }

    pub fn filter_rect(&self) -> Option<Rect> {
        if !matches!(self.edit, Edit::Filter(_)) && self.filter.is_empty() {
            return None;
        }
        let w = self.px(220.0);
        let l = self.layout();
        Some(Rect::new(l.toolbar.right() - w - self.px(8.0), self.px(7.0), w, l.toolbar.h - self.px(14.0)))
    }

    /// The path's clickable parts: (start x, width, folder), trimmed from the left to fit.
    pub fn crumbs(&mut self) -> Vec<(i32, i32, PathBuf)> {
        let start = self.px(6.0) + self.px(36.0) * 3 + self.px(8.0);
        let end = self.filter_rect().map_or(self.w - self.px(8.0), |r| r.x - self.px(8.0));
        let end = self.empty_trash_rect().map_or(end, |r| r.x - self.px(8.0));
        let mut parts: Vec<(String, PathBuf)> = Vec::new();
        let mut acc = PathBuf::new();
        // Inside any Trash, its real path is noise: it starts at "Trash".
        let mut components = self.dir.components();
        let files = self.trash_files_root(&self.dir);
        if let Some((f, rest)) = files.as_ref().and_then(|f| Some((f, self.dir.strip_prefix(f).ok()?))) {
            // "Trash" opens the Trash view, and what's below it is where it really is.
            parts.push(("Trash".into(), self.trash_dir.clone()));
            acc = f.clone();
            components = rest.components();
        }
        for c in components {
            acc.push(c);
            let label = match c {
                std::path::Component::RootDir => "/".to_string(),
                other => other.as_os_str().to_string_lossy().into_owned(),
            };
            parts.push((label, acc.clone()));
        }
        let gap = self.px(14.0) as f32;
        let widths: Vec<f32> = parts.iter().map(|(s, _)| self.text.measure(s) + self.px(12.0) as f32).collect();
        let room = (end - start) as f32;
        let mut first = 0;
        let mut total: f32 = widths.iter().sum::<f32>() + gap * parts.len().saturating_sub(1) as f32;
        while total > room && first + 1 < parts.len() {
            total -= widths[first] + gap;
            first += 1;
        }
        let mut x = start as f32;
        let mut out = Vec::new();
        for (i, (_, path)) in parts.into_iter().enumerate().skip(first) {
            out.push((x as i32, widths[i] as i32, path));
            x += widths[i] + gap;
        }
        out
    }

    /// What a folder is called in the path bar and the window title.
    pub fn crumb_label(&self, path: &Path) -> String {
        if path == self.trash_dir {
            return "Trash".into();
        }
        title_for(path)
    }

    fn crumb_at(&mut self, x: i32) -> Option<usize> {
        self.crumbs().iter().position(|(cx, w, _)| x >= *cx && x < cx + w)
    }

    fn set_hover(&mut self, h: Hit) {
        // A drag has its own cursor and highlight, and scrolling mustn't replace them.
        if h == self.hover || matches!(self.file_drag, Some(FileDrag::Active(_))) || matches!(&self.file_drag, Some(FileDrag::Band(b)) if b.active) {
            return;
        }
        let old = std::mem::replace(&mut self.hover, h);
        for hit in [old, h] {
            match hit {
                Hit::Row(r) => self.invalidate_row(r),
                Hit::None => {}
                _ => self.invalidate_chrome(),
            }
        }
        self.set_cursor(cursor_for(h));
    }

    /// The only place the cursor is asked for. The pointer moves far more often than it changes.
    fn set_cursor(&mut self, c: Cursor) {
        if self.pointer_cursor != c {
            self.pointer_cursor = c;
            self.cmds.push(Cmd::Cursor(c));
        }
    }

    fn invalidate_chrome(&mut self) {
        if self.menu.is_some() || self.dialog.is_some() {
            self.invalidate_all();
            return;
        }
        let l = self.layout();
        self.invalidate(l.toolbar);
        self.invalidate(l.sidebar);
        self.invalidate(l.header);
        self.invalidate(l.notices);
    }

    fn invalidate_row(&mut self, row: usize) {
        let l = self.layout();
        let y = l.list.y + (row as f64 * l.row_h as f64 - self.scroll) as i32;
        self.invalidate(Rect::new(l.list.x, y, l.list.w, l.row_h).intersect(l.list));
    }

    fn set_pointer(&mut self, x: f64, y: f64) {
        self.pointer = ((x * self.scale) as i32, (y * self.scale) as i32);
    }

    fn pointer_moved(&mut self, x: f64, y: f64) {
        self.set_pointer(x, y);
        let (px, py) = self.pointer;
        if self.preview_drag(px, py) {
            return;
        }
        if let Some(FileDrag::Pressed { at, band, .. }) = &self.file_drag {
            let (at, beside_name) = (*at, band.is_some());
            let reach = self.px(DRAG_START);
            if (px - at.0).abs() >= reach || (py - at.1).abs() >= reach {
                if beside_name {
                    self.band_from_press();
                } else {
                    self.start_file_drag();
                }
            }
        }
        if let Some(FileDrag::Band(b)) = &self.file_drag {
            let (at, active) = (b.at, b.active);
            let reach = self.px(DRAG_START);
            if !active && ((px - at.0).abs() >= reach || (py - at.1).abs() >= reach) {
                self.band_activate();
            }
            if matches!(&self.file_drag, Some(FileDrag::Band(b)) if b.active) {
                return self.band_update();
            }
        }
        if matches!(&self.file_drag, Some(FileDrag::Active(d)) if !d.wayland) && (px < 0 || py < 0 || px >= self.w || py >= self.h) {
            self.start_wayland_drag();
            return;
        }
        // Pointer events stop during a compositor's drag, so these mean it refused this one.
        if matches!(&self.file_drag, Some(FileDrag::Active(d)) if d.wayland && d.handed_over.is_some_and(|t| t.elapsed() > WAYLAND_DRAG_GRACE)) {
            self.cancel_file_drag();
        }
        if matches!(&self.file_drag, Some(FileDrag::Active(d)) if d.wayland) {
            return;
        }
        if matches!(self.file_drag, Some(FileDrag::Active(_))) {
            self.refresh_file_drag();
            // The refresh can end it, and then this move belongs to whatever's on top now.
            if matches!(self.file_drag, Some(FileDrag::Active(_))) {
                return;
            }
        }
        if let Some(grab) = self.drag_scroll {
            let l = self.layout();
            let (_, h) = self.scrollbar_thumb();
            let travel = (l.list.h - h).max(1) as f64;
            let top = (py - l.list.y - grab) as f64;
            self.scroll_to(top / travel * self.max_scroll());
            return;
        }
        let h = self.hit(px, py);
        if let Hit::MenuItem(i) = h
            && let Some(m) = self.menu.as_mut()
            && m.at != i
        {
            m.at = i;
            self.invalidate_all();
        }
        self.set_hover(h);
    }

    fn button(&mut self, button: u32, pressed: bool) {
        if !pressed {
            if button == BTN_LEFT {
                self.drag_scroll = None;
                self.preview_release();
                self.finish_file_drag();
            }
            return;
        }
        // The release of a drag can go missing if the compositor takes the pointer away.
        self.cancel_file_drag();
        // A popup that hasn't appeared yet can't be what this click was aimed at.
        if self.preview.is_some() && self.preview_rect().is_empty() {
            self.close_preview();
        }
        let (x, y) = self.pointer;
        let hit = self.hit(x, y);
        if self.dialog.is_some() {
            if button == BTN_LEFT && !self.dialog_guarded() {
                match hit {
                    Hit::DialogButton(i) => self.press_dialog_button(i),
                    Hit::DialogCheck => {
                        if let Some(Dialog::Conflict { apply_to_all, .. }) = self.dialog.as_mut() {
                            *apply_to_all = !*apply_to_all;
                            self.invalidate_all();
                        }
                    }
                    _ => {}
                }
            }
            return;
        }
        if self.preview.is_some() {
            return self.preview_button(button, hit);
        }
        match button {
            BTN_SIDE => return self.go_back(),
            BTN_EXTRA => return self.go_forward(),
            _ => {}
        }
        // A click anywhere else ends an edit and keeps what was typed for a filter.
        if !matches!(hit, Hit::PathBar | Hit::FilterBox) && !matches!(self.edit, Edit::None) {
            // A click away commits a rename or new folder, as other file managers do.
            match self.edit {
                Edit::Path(_) => self.field_cancel(),
                Edit::Rename { .. } | Edit::NewFolder(_) => self.field_commit(),
                _ => self.set_edit(Edit::None),
            }
        }
        if button == BTN_RIGHT {
            if self.menu.is_some() {
                self.close_menu();
            }
            if let Hit::Row(row) = self.hit(x, y) {
                // The menu's about this row, selected or not, so the cursor comes here either way.
                let keep_selection = self.is_selected(self.rows[row]);
                self.move_cursor(row, Mods { ctrl: keep_selection, ..Mods::default() });
                self.open_menu(x, y);
            } else if matches!(self.hit(x, y), Hit::ListEmpty) {
                self.clear_selection();
                self.invalidate_list();
                self.open_menu(x, y);
            }
            return;
        }
        if button != BTN_LEFT {
            return;
        }
        match hit {
            Hit::MenuItem(i) => {
                let item = self.menu.as_ref().and_then(|m| m.items.get(i)).filter(|i| i.enabled).map(|i| i.action);
                self.close_menu();
                if let Some(a) = item {
                    self.action(a);
                }
            }
            Hit::Outside => self.close_menu(),
            Hit::Back => self.go_back(),
            Hit::Forward => self.go_forward(),
            Hit::Up => self.go_up(),
            Hit::EmptyTrash => self.action(Action::EmptyTrash),
            Hit::Crumb(i) => {
                if let Some((_, _, path)) = self.crumbs().into_iter().nth(i)
                    && path != self.dir
                {
                    let name = if path == self.trash_dir && let Some(files) = self.trash_files_root(&self.dir) {
                        // The Trash view finds rows by their real path, not their name.
                        self.dir.strip_prefix(&files).ok().and_then(|r| r.components().next()).map(|c| files.join(c).into_os_string().into_vec())
                    } else {
                        self.dir.strip_prefix(&path).ok().and_then(|r| r.components().next()).map(|c| std::os::unix::ffi::OsStrExt::as_bytes(c.as_os_str()).to_vec())
                    };
                    self.navigate(path, name);
                }
            }
            Hit::PathBar => {
                if matches!(self.edit, Edit::Path(_)) {
                    self.click_in_field(x);
                } else {
                    self.start_path_edit();
                }
            }
            Hit::FilterBox => {
                if matches!(self.edit, Edit::Filter(_)) {
                    self.click_in_field(x);
                } else {
                    self.start_filter("");
                }
            }
            Hit::Place(i) => {
                if let Some(p) = self.places.get(i).map(|p| p.path.clone()) {
                    self.navigate(p, None);
                }
            }
            Hit::Header(key) => self.action(Action::SortBy(key)),
            Hit::Scrollbar => {
                let l = self.layout();
                let (top, h) = self.scrollbar_thumb();
                let inside = y - l.list.y - top;
                if inside >= 0 && inside < h {
                    self.drag_scroll = Some(inside);
                } else {
                    // A click on the track jumps there, and the thumb follows the pointer.
                    self.drag_scroll = Some(h / 2);
                    self.pointer_moved(x as f64 / self.scale, y as f64 / self.scale);
                }
            }
            Hit::NoticeClose(i) => {
                if i < self.notices.len() {
                    self.notices.remove(i);
                    self.clamp_scroll();
                    self.invalidate_all();
                }
            }
            Hit::JobCancel(i) => {
                if let Some(j) = self.visible_jobs().nth(i) {
                    j.job.cancel();
                }
            }
            Hit::DialogButton(_) | Hit::DialogCheck | Hit::PreviewBody => {}
            Hit::Row(row) => {
                // Read first: a press that opens the row changes what's listed.
                let name = self.listing.as_ref().map_or_else(Vec::new, |l| l.key(self.rows[row]).into_owned());
                let band = (!self.on_row_name(row, x)).then(|| (self.save_selection(), self.list_y(y)));
                if let Some(narrow) = self.click_row(row) {
                    self.file_drag = Some(FileDrag::Pressed { at: (x, y), row, dir: self.dir.clone(), name, narrow, band });
                }
            }
            Hit::ListEmpty => {
                let saved = self.save_selection();
                if !saved.add {
                    self.selection_epoch += 1;
                    self.clear_selection();
                    self.cursor_shown = false;
                    self.invalidate_list();
                }
                let start_y = self.list_y(y);
                self.file_drag = Some(FileDrag::Band(Band { at: (x, y), active: false, start_y, saved, range: None, rect: Rect::default(), scroll_at: None }));
            }
            Hit::None => {}
        }
    }

    /// The box the path or filter field draws in, for placing the caret from a click.
    pub fn path_box(&mut self) -> Rect {
        let l = self.layout();
        let start = self.px(6.0) + self.px(36.0) * 3 + self.px(8.0);
        let end = self.filter_rect().map_or(self.w - self.px(8.0), |r| r.x - self.px(8.0));
        let end = self.empty_trash_rect().map_or(end, |r| r.x - self.px(8.0));
        Rect::new(start, self.px(7.0), end - start, l.toolbar.h - self.px(14.0))
    }

    fn click_in_field(&mut self, x: i32) {
        let r = if matches!(self.edit, Edit::Filter(_)) { self.filter_rect().unwrap_or_default() } else { self.path_box() };
        let text = self.field_mut().map(|f| (f.text.clone(), f.caret));
        let Some((text, caret)) = text else { return };
        // The same scroll the painter applies when the text is longer than the box.
        let inner = r.w - self.px(16.0);
        let shift = (self.text.measure(&text[..caret]) - inner as f32 + 2.0).max(0.0);
        let at = self.text.offset_at(&text, (x - r.x - self.px(8.0)) as f32 + shift);
        if let Some(f) = self.field_mut() {
            f.move_to(at, false);
        }
        self.invalidate(self.layout().toolbar);
    }

    /// Returns whether a release should narrow the selection to this row, or `None` if the
    /// press opened it and there's nothing to drag.
    fn click_row(&mut self, row: usize) -> Option<bool> {
        let now = Instant::now();
        // Compared by key: a reload between the clicks can move rows.
        let name = self.listing.as_ref().map_or_else(Vec::new, |l| l.key(self.rows[row]).into_owned());
        let double = self.last_click.as_ref().is_some_and(|(t, n)| *n == name && now.duration_since(*t) < DOUBLE_CLICK);
        let (ctrl, shift) = (self.mods_now.ctrl, self.mods_now.shift);
        if double && !ctrl && !shift {
            self.last_click = None;
            self.move_cursor(row, Mods::default());
            self.action(Action::Open);
            return None;
        }
        self.last_click = Some((now, name));
        // Pressing one row of a selection might be the start of dragging all of it.
        if !ctrl && !shift && self.sel_count > 1 && self.is_selected(self.rows[row]) {
            return Some(true);
        }
        if ctrl {
            self.selection_epoch += 1;
            let id = self.rows[row];
            self.set_selected(id, !self.is_selected(id));
            self.cursor = row;
            self.anchor = row;
            self.cursor_shown = true;
            self.invalidate_list();
        } else {
            self.move_cursor(row, Mods { shift, ..Mods::default() });
        }
        Some(false)
    }

    // ---- Selecting with a box ----

    /// Whether a position is on a row's icon or name, which is where a file drag starts.
    fn on_row_name(&mut self, row: usize, x: i32) -> bool {
        let Some(name) = self.listing.as_ref().zip(self.rows.get(row)).map(|(l, &id)| String::from_utf8_lossy(l.name(id)).into_owned()) else { return false };
        let l = self.layout();
        let name_x = l.list.x + self.px(38.0);
        let name_max = l.list.right() - self.px(14.0) - l.col_date - l.col_size - self.px(10.0) - name_x;
        let width = (self.text.measure(&name).ceil() as i32).min(name_max);
        x >= l.list.x + self.px(10.0) && x < name_x + width + self.px(6.0)
    }

    fn save_selection(&self) -> SavedSelection {
        SavedSelection { selected: self.selected.clone(), count: self.sel_count, cursor: self.cursor, anchor: self.anchor, shown: self.cursor_shown, add: self.mods_now.ctrl || self.mods_now.shift }
    }

    /// The rows are about to change. A box, or a press that could become one, holds row numbers
    /// and a selection that only mean anything for the rows as they are. What a box selected
    /// so far is kept like any other selection.
    fn rows_changing(&mut self) {
        match &self.file_drag {
            Some(FileDrag::Band(_)) => self.cancel_file_drag(),
            Some(FileDrag::Pressed { band: Some(_), .. }) => self.file_drag = None,
            _ => {}
        }
    }

    /// A height on screen, measured from the top of the whole list.
    fn list_y(&self, y: i32) -> f64 {
        (y - self.layout().list.y) as f64 + self.scroll
    }

    fn band_from_press(&mut self) {
        let Some(FileDrag::Pressed { at, row, dir, name, band: Some((saved, start_y)), .. }) = self.file_drag.take() else { return };
        if self.pressed_row_is_current(row, &dir, &name) {
            self.file_drag = Some(FileDrag::Band(Band { at, active: false, start_y, saved, range: None, rect: Rect::default(), scroll_at: None }));
            self.band_activate();
        }
    }

    fn band_activate(&mut self) {
        // Before it counts as active, which keeps the hover as it is.
        self.set_hover(Hit::None);
        let Some(FileDrag::Band(b)) = &mut self.file_drag else { return };
        b.active = true;
        let (add, count) = (b.saved.add, b.saved.count);
        if add {
            self.selected.clone_from(&b.saved.selected);
            self.sel_count = count;
        } else {
            self.clear_selection();
        }
        self.selection_epoch += 1;
        // A drag wasn't a click, so the next press can't be the second half of a double-click.
        self.last_click = None;
        self.invalidate_list();
    }

    /// Brings the box and the selection up to date with the pointer and the scroll.
    fn band_update(&mut self) {
        let Some(FileDrag::Band(mut b)) = self.file_drag.take() else { return };
        let l = self.layout();
        let (px, py) = self.pointer;
        let row_h = l.row_h.max(1) as f64;
        let start = (b.start_y / row_h) as usize;
        let end = (self.list_y(py).max(0.0) / row_h) as usize;
        let last = self.rows.len().saturating_sub(1);
        // A box that's entirely below the last row touches nothing.
        let range = (start.min(end) < self.rows.len()).then(|| (start.min(end), start.max(end).min(last)));
        if range != b.range {
            let leaving = rows_outside(b.range, range);
            let joining = rows_outside(range, b.range);
            let changed = leaving.iter().chain(&joining).map(ExactSizeIterator::len).sum::<usize>();
            for r in leaving.iter().cloned().flatten() {
                let id = self.rows[r];
                let was = b.saved.add && b.saved.selected.get(id as usize / 64).is_some_and(|w| w >> (id % 64) & 1 == 1);
                self.set_selected(id, was);
            }
            for r in joining.iter().cloned().flatten() {
                self.set_selected(self.rows[r], true);
            }
            if changed > 0 {
                self.selection_epoch += 1;
            }
            let old_cursor = self.cursor;
            if range.is_some() {
                self.anchor = start.min(last);
                self.cursor = end.min(last);
                self.cursor_shown = true;
            } else {
                // Touching nothing, so nothing's under the cursor because of this box.
                self.anchor = b.saved.anchor;
                self.cursor = b.saved.cursor;
                self.cursor_shown = b.saved.add && b.saved.shown;
            }
            // Past a screenful, it's all of what shows.
            if changed > (l.list.h / l.row_h.max(1)) as usize {
                self.invalidate(l.list);
            } else {
                for r in leaving.iter().chain(&joining).cloned().flatten() {
                    self.invalidate_row(r);
                }
                self.invalidate_row(old_cursor);
                self.invalidate_row(self.cursor);
            }
            self.invalidate(l.status);
            b.range = range;
        }
        let top = (b.start_y - self.scroll) as i32 + l.list.y;
        let rect = Rect::new(b.at.0.min(px), top.min(py), (b.at.0 - px).abs() + 1, (top - py).abs() + 1).intersect(l.list);
        self.invalidate(b.rect);
        self.invalidate(rect);
        b.rect = rect;
        let can_scroll = (py < l.list.y && self.scroll > 0.0) || (py >= l.list.bottom() && self.scroll < self.max_scroll());
        b.scroll_at = if can_scroll { b.scroll_at.or_else(|| Some(Instant::now() + BAND_SCROLL_EVERY)) } else { None };
        self.file_drag = Some(FileDrag::Band(b));
    }

    fn band_on_timer(&mut self, now: Instant) {
        let Some(FileDrag::Band(b)) = &mut self.file_drag else { return };
        if !b.scroll_at.is_some_and(|t| t <= now) {
            return;
        }
        b.scroll_at = Some(now + BAND_SCROLL_EVERY);
        let l = self.layout();
        let y = self.pointer.1;
        // Faster the further past the end it is, up to a few rows at a time.
        let past = if y < l.list.y { y - l.list.y } else { y - l.list.bottom() + 1 };
        let step = (past as f64 / 4.0).abs().clamp(2.0, l.row_h as f64 * 3.0).copysign(past as f64);
        let old = self.scroll;
        self.scroll_to(old + step);
        // Nowhere further to go, so the scroll didn't update it, and this stops the timer.
        if self.scroll == old {
            self.band_update();
        }
    }

    /// Ends a box, keeping what it selected.
    fn band_ended(&mut self, b: &Band) {
        self.invalidate(b.rect);
        let (x, y) = self.pointer;
        let h = self.hit(x, y);
        self.set_hover(h);
    }

    /// Cancels a box, and the selection goes back to what it was before the press.
    fn band_put_back(&mut self) {
        let Some(FileDrag::Band(b)) = self.file_drag.take() else { return };
        self.selected = b.saved.selected.clone();
        self.sel_count = b.saved.count;
        self.cursor = b.saved.cursor;
        self.anchor = b.saved.anchor;
        self.cursor_shown = b.saved.shown;
        self.selection_epoch += 1;
        self.invalidate_list();
        self.band_ended(&b);
    }

    // ---- Dragging files ----

    /// Whether a pressed row is still what's at that position in the same folder.
    fn pressed_row_is_current(&self, row: usize, dir: &Path, name: &[u8]) -> bool {
        self.dir == dir && self.rows.get(row).zip(self.listing.as_ref()).is_some_and(|(&id, l)| *l.key(id) == *name)
    }

    fn start_file_drag(&mut self) {
        let Some(FileDrag::Pressed { row, dir, name, .. }) = self.file_drag.take() else { return };
        // Ctrl can have just unselected the pressed row, and then there's nothing to carry.
        if !self.pressed_row_is_current(row, &dir, &name) || !self.is_selected(self.rows[row]) {
            return;
        }
        let paths = self.selected_paths();
        if paths.is_empty() {
            return;
        }
        let kind = self.listing.as_ref().map_or(Kind::File, |l| l.kind(self.rows[row]));
        let ghost = DragGhost { name, kind, count: paths.len() };
        // A drag wasn't a click, so the next press can't be the second half of a double-click.
        self.last_click = None;
        self.set_hover(Hit::None);
        self.next_drag += 1;
        let id = self.next_drag;
        let (probe, asked) = std::sync::mpsc::channel::<PathBuf>();
        let hub = self.hub.clone();
        let from = dir.clone();
        // It ends with the drag, when the sender goes, unless a disk has it stuck.
        let _ = std::thread::Builder::new().name("gannet-drag".into()).spawn(move || {
            hub.send(Msg::DragSource { drag: id, facts: source_facts(&dir) });
            for path in asked {
                let device = std::fs::metadata(&path).ok().map(|m| std::os::unix::fs::MetadataExt::dev(&m));
                hub.send(Msg::DragDevice { drag: id, path, device });
            }
        });
        self.file_drag = Some(FileDrag::Active(ActiveDrag { paths, dir: from, probe, id, source: None, devices: HashMap::new(), target: None, label: Rect::default(), ghost, ghost_at: Rect::default(), wayland: false, handed_over: None, inside: false, moved_inside: false, accepting: false }));
    }

    /// The folder a drop here would land in. Dropping on what's being dragged, on the folder
    /// it's already in, or inside itself isn't a target.
    fn drop_target_at(&mut self, d: &ActiveDrag, x: i32, y: i32) -> Option<DropTarget> {
        let hit = self.hit(x, y);
        match hit {
            Hit::Row(row) => {
                let l = self.listing.as_ref()?;
                let id = *self.rows.get(row)?;
                if self.in_trash() || !l.kind(id).is_dirlike() || self.is_selected(id) {
                    return None;
                }
                Some(DropTarget { hit, dest: l.path(id), trash: false })
            }
            Hit::Place(i) => {
                let place = self.places.get(i)?;
                let trash = place.kind == PlaceKind::Trash;
                // Ctrl asks for a copy, and trashing the originals would be the opposite.
                if place.path == self.dir || (trash && (self.in_trash() || self.mods_now.ctrl)) {
                    return None;
                }
                let inside_itself = place.path.strip_prefix(&self.dir).ok().and_then(|rest| rest.components().next()).is_some_and(|first| d.paths.iter().any(|p| p.file_name() == Some(first.as_os_str())));
                if inside_itself {
                    return None;
                }
                Some(DropTarget { hit, dest: place.path.clone(), trash })
            }
            _ => None,
        }
    }

    pub(crate) fn drop_action(&self, d: &ActiveDrag, t: &DropTarget) -> DropAction {
        use gannet_core::ops::{DropVerb, drop_verb};
        if t.trash {
            return DropAction::Trash;
        }
        // The Trash's items can only leave as copies, as with cut and paste.
        if self.in_trash() {
            return DropAction::Copy;
        }
        let (device, can_delete) = d.source.unzip();
        let same_disk = device.zip(d.devices.get(&t.dest).copied().flatten()).map(|(a, b)| a == b);
        match drop_verb(self.mods_now.ctrl, self.mods_now.shift, same_disk, can_delete.unwrap_or(false)) {
            DropVerb::Move => DropAction::Move,
            DropVerb::Copy => DropAction::Copy,
            DropVerb::Link => DropAction::Link,
        }
    }

    /// What the drag is carrying, and what dropping it now would do.
    pub(crate) fn drag_label(&self, d: &ActiveDrag) -> String {
        let what = match d.paths.as_slice() {
            [one] => format!("“{}”", one.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned())),
            many => format!("{} items", many.len()),
        };
        let Some(t) = &d.target else { return what };
        let folder = t.dest.file_name().map_or_else(|| t.dest.to_string_lossy().into_owned(), |n| n.to_string_lossy().into_owned());
        match self.drop_action(d, t) {
            DropAction::Move => format!("Move {what} to {folder}"),
            DropAction::Copy => format!("Copy {what} to {folder}"),
            DropAction::Link => format!("Link {what} in {folder}"),
            DropAction::Trash => format!("Move {what} to Trash"),
        }
    }

    /// A label for a compositor's drag, which has a picture hanging below the pointer. It goes
    /// under the picture, or above the pointer if there's no room left there.
    fn drag_label_rect(&mut self, label: &str, picture: f64) -> Rect {
        let (x, y) = self.pointer;
        let (x, below) = (x + self.px(14.0), y + self.px(picture));
        if below + self.px(26.0) <= self.h {
            self.label_rect_at(label, x, below)
        } else {
            self.label_rect_at(label, x, y - self.px(34.0))
        }
    }

    fn label_rect_at(&mut self, label: &str, x: i32, y: i32) -> Rect {
        let w = (self.text.measure(label).ceil() as i32 + self.px(16.0)).min(self.px(420.0)).min(self.w);
        let h = self.px(26.0);
        Rect::new(x.min(self.w - w).max(0), y.min(self.h - h).max(0), w, h)
    }

    /// The size of the row a drag carries, with room for the stack behind it if it's several.
    pub(crate) fn ghost_size(&mut self, g: &DragGhost) -> (i32, i32) {
        let name = String::from_utf8_lossy(&g.name);
        let badge = if g.count > 1 { self.text.measure(&g.count.to_string()).ceil() as i32 + self.px(20.0) } else { 0 };
        let w = self.text.measure(&name).ceil() as i32 + self.px(52.0) + badge;
        let stack = if g.count > 1 { self.px(4.0) } else { 0 };
        (w.clamp(self.px(120.0), self.px(320.0)).min(self.w.max(1)) + stack, self.px(26.0) + stack)
    }

    /// Draws what the pointer carries for a drag out of this window, for the compositor to show.
    fn render_drag_icon(&mut self, g: &DragGhost) -> Option<crate::wl::DragIcon> {
        let (w, h) = self.ghost_size(g);
        if w <= 0 || h <= 0 {
            return None;
        }
        let mut pixels = vec![self.colors.base; (w * h) as usize];
        let mut canvas = crate::canvas::Canvas::new(&mut pixels, w, h);
        canvas.set_clip(Rect::new(0, 0, w, h));
        self.paint_ghost(&mut canvas, g, Rect::new(0, 0, w, h));
        Some(crate::wl::DragIcon { w, h, pixels })
    }

    /// Brings the drag up to date with the pointer, the keys and whatever's under them now.
    fn refresh_file_drag(&mut self) {
        // A menu or dialog that opens takes over the pointer and the keys, and a press that
        // hasn't become a drag yet goes with it.
        if self.menu.is_some() || self.dialog.is_some() || self.preview.is_some() {
            self.cancel_file_drag();
            return;
        }
        // Checked before taking it: a press that hasn't become a drag yet has to survive this.
        if !matches!(self.file_drag, Some(FileDrag::Active(_))) {
            return;
        }
        let Some(FileDrag::Active(mut d)) = self.file_drag.take() else { return };
        // The folder can go away under a drag, and what it carried isn't what's showing now.
        if d.dir != self.dir {
            // Out over other programs, the compositor still has it, and could still drop it.
            if d.wayland {
                self.cmds.push(Cmd::CancelDrag);
            }
            self.end_active_drag(&d);
            return;
        }
        // Out over other programs, there's nothing here to show.
        if d.wayland && !(d.inside && d.moved_inside) {
            self.file_drag = Some(FileDrag::Active(d));
            return;
        }
        let (x, y) = self.pointer;
        let target = self.drop_target_at(&d, x, y);
        if target.as_ref().map(|t| t.hit) != d.target.as_ref().map(|t| t.hit) {
            for t in [d.target.as_ref(), target.as_ref()].into_iter().flatten() {
                self.invalidate_drop_target(t.hit);
            }
        }
        d.target = target;
        if let Some(t) = &d.target
            && !t.trash
            && !d.devices.contains_key(&t.dest)
        {
            d.devices.insert(t.dest.clone(), None);
            let _ = d.probe.send(t.dest.clone());
        }
        let cursor = d.target.as_ref().map_or(Cursor::NoDrop, |t| match self.drop_action(&d, t) {
            DropAction::Move | DropAction::Trash => Cursor::Move,
            DropAction::Copy => Cursor::Copy,
            DropAction::Link => Cursor::Alias,
        });
        if d.wayland {
            // The compositor draws the cursor during its drags, and it decides by this.
            if d.accepting != d.target.is_some() {
                d.accepting = d.target.is_some();
                self.cmds.push(Cmd::AcceptDrop(d.accepting));
            }
        } else {
            self.set_cursor(cursor);
        }
        self.invalidate(d.label);
        self.invalidate(d.ghost_at);
        // In the compositor's hands, it draws the picture. Here, it follows the pointer.
        let (px, py) = self.pointer;
        d.ghost_at = if d.wayland {
            Rect::default()
        } else {
            let (w, h) = self.ghost_size(&d.ghost);
            Rect::new((px + self.px(14.0)).min(self.w - w).max(0), (py + self.px(10.0)).min(self.h - h).max(0), w, h)
        };
        // Only what a drop would do needs saying: the row already shows what's being carried.
        d.label = if d.target.is_some() {
            let label = self.drag_label(&d);
            if d.wayland { self.drag_label_rect(&label, GANNET_PICTURE) } else { self.label_rect_at(&label, d.ghost_at.x, d.ghost_at.bottom() + self.px(4.0)) }
        } else {
            Rect::default()
        };
        self.invalidate(d.label);
        self.invalidate(d.ghost_at);
        self.file_drag = Some(FileDrag::Active(d));
    }

    fn invalidate_drop_target(&mut self, hit: Hit) {
        match hit {
            Hit::Row(r) => self.invalidate_row(r),
            Hit::None => {}
            _ => self.invalidate(self.layout().sidebar),
        }
    }

    fn end_active_drag(&mut self, d: &ActiveDrag) {
        self.invalidate(d.label);
        self.invalidate(d.ghost_at);
        if let Some(t) = &d.target {
            self.invalidate_drop_target(t.hit);
        }
        let (x, y) = self.pointer;
        let h = self.hit(x, y);
        self.set_hover(h);
        // The hover can be unchanged while the drag's cursor still has to go.
        self.set_cursor(cursor_for(h));
    }

    pub(crate) fn cancel_file_drag(&mut self) {
        match self.file_drag.take() {
            Some(FileDrag::Active(d)) => {
                if d.wayland {
                    self.cmds.push(Cmd::CancelDrag);
                }
                self.end_active_drag(&d);
            }
            Some(FileDrag::Band(b)) => self.band_ended(&b),
            _ => {}
        }
    }

    /// The pointer has left the window with the button down, so other programs can have it.
    fn start_wayland_drag(&mut self) {
        let Some(FileDrag::Active(d)) = &mut self.file_drag else { return };
        d.wayland = true;
        d.handed_over = Some(Instant::now());
        // It's gone from this window, so nothing here is under it until it comes back.
        self.pointer = (-1, -1);
        let (old_target, old_label) = (d.target.take().map(|t| t.hit), std::mem::take(&mut d.label));
        let old_ghost = std::mem::take(&mut d.ghost_at);
        let formats = clipboard::drag_formats(&d.paths);
        let ghost = d.ghost.clone();
        let source = d.source;
        self.invalidate(old_label);
        self.invalidate(old_ghost);
        if let Some(hit) = old_target {
            self.invalidate_drop_target(hit);
        }
        // Other programs can only do what's offered. Plain copies, and Shift moves. Another
        // Gannet window has the marker, and uses the same rule as inside a window. The Trash's
        // items only leave as copies, because its two folders have to stay paired, so they get
        // no marker. Nor does a drag whose folder hasn't answered yet, which then just copies.
        let move_only = self.mods_now.shift && !self.mods_now.ctrl && !self.in_trash();
        let marker = source.filter(|_| !self.in_trash()).map(|(device, can_delete)| clipboard::DragMarker { device, can_delete, ctrl: self.mods_now.ctrl, shift: self.mods_now.shift });
        let icon = self.render_drag_icon(&ghost);
        self.cmds.push(Cmd::StartDrag { formats, marker, move_only, icon });
    }

    /// The position that comes with a drag's arrival can't be believed: Hyprland always says
    /// the middle of the window. So nothing under it lights up or gets a label, and the real
    /// position comes with the first motion, right behind it.
    fn drag_entered(&mut self, x: f64, y: f64, files: bool, can_copy: bool, can_move: bool, marker: Option<clipboard::DragMarker>) {
        // A drag arriving means the button came up without this window hearing about it.
        if matches!(self.file_drag, Some(FileDrag::Band(_) | FileDrag::Pressed { .. })) {
            self.cancel_file_drag();
        }
        if let Some(FileDrag::Active(d)) = &mut self.file_drag
            && d.wayland
        {
            // Back over its own window. It can be accepted from the first motion, when it's
            // known what's under it.
            d.inside = true;
            d.moved_inside = false;
            d.accepting = false;
            return;
        }
        self.set_pointer(x, y);
        self.next_drag += 1;
        let id = self.next_drag;
        let probe = marker.map(|_| {
            let (probe, asked) = std::sync::mpsc::channel::<PathBuf>();
            let hub = self.hub.clone();
            // It ends with the drag, when the sender goes, unless a disk has it stuck.
            let _ = std::thread::Builder::new().name("gannet-drag".into()).spawn(move || {
                for path in asked {
                    let device = std::fs::metadata(&path).ok().map(|m| std::os::unix::fs::MetadataExt::dev(&m));
                    hub.send(Msg::DragDevice { drag: id, path, device });
                }
            });
            probe
        });
        self.incoming = Some(IncomingDrag { files, can_copy, can_move, marker, probe, id, devices: HashMap::new(), moved: false, target: None, label: Rect::default(), accepting: false });
        self.refresh_incoming();
    }

    fn drag_left(&mut self) {
        if let Some(FileDrag::Active(d)) = &mut self.file_drag {
            d.inside = false;
            d.accepting = false;
            let (old_target, old_label) = (d.target.take().map(|t| t.hit), std::mem::take(&mut d.label));
            self.invalidate(old_label);
            if let Some(hit) = old_target {
                self.invalidate_drop_target(hit);
            }
        }
        if let Some(i) = self.incoming.take() {
            self.end_incoming(&i);
        }
    }

    fn end_incoming(&mut self, i: &IncomingDrag) {
        self.invalidate(i.label);
        if let Some(t) = &i.target {
            self.invalidate_drop_target(t.hit);
        }
    }

    /// Where a drop from elsewhere would land. With no motion yet, the pointer's position
    /// isn't known, so that's the folder showing, whatever row or place seems to be there.
    fn incoming_target(&mut self, i: &IncomingDrag) -> Option<DropTarget> {
        if !i.files {
            return None;
        }
        if !i.moved {
            let refused = self.in_trash() || self.dialog.is_some() || self.menu.is_some() || self.preview.is_some();
            return (!refused).then(|| DropTarget { hit: Hit::None, dest: self.dir.clone(), trash: false });
        }
        let (x, y) = self.pointer;
        self.incoming_target_at(self.incoming_can_trash(i), x, y)
    }

    /// Where a drop from elsewhere would land. A folder or place under the pointer, and
    /// otherwise the folder showing.
    fn incoming_target_at(&mut self, can_trash: bool, x: i32, y: i32) -> Option<DropTarget> {
        let hit = self.hit(x, y);
        match hit {
            Hit::Row(_) | Hit::ListEmpty if self.in_trash() => None,
            Hit::Row(row) => {
                let l = self.listing.as_ref()?;
                let id = *self.rows.get(row)?;
                if l.kind(id).is_dirlike() {
                    return Some(DropTarget { hit, dest: l.path(id), trash: false });
                }
                Some(DropTarget { hit: Hit::ListEmpty, dest: self.dir.clone(), trash: false })
            }
            Hit::ListEmpty => Some(DropTarget { hit, dest: self.dir.clone(), trash: false }),
            Hit::Place(i) => {
                let place = self.places.get(i)?;
                let trash = place.kind == PlaceKind::Trash;
                // Trashing takes the files away, so the other program has to allow that.
                if trash && !can_trash {
                    return None;
                }
                Some(DropTarget { hit, dest: place.path.clone(), trash })
            }
            _ => None,
        }
    }

    /// The keys that count for a drag from another Gannet window: the ones held now if this
    /// window can see them, and otherwise the ones held when the drag left its window.
    fn marker_keys(&self, m: &clipboard::DragMarker) -> (bool, bool) {
        if self.has_keyboard { (self.mods_now.ctrl, self.mods_now.shift) } else { (m.ctrl, m.shift) }
    }

    /// Whether a drop here can go to the Trash. Ctrl asks for a copy, and trashing the
    /// originals would be the opposite.
    fn incoming_can_trash(&self, i: &IncomingDrag) -> bool {
        match &i.marker {
            Some(m) => !self.marker_keys(m).0,
            None => i.can_move,
        }
    }

    /// What dropping here does. The same rule as inside a window for another Gannet window,
    /// and whatever the other program asks for otherwise.
    pub(crate) fn incoming_action(&self, i: &IncomingDrag, t: &DropTarget) -> DropAction {
        use gannet_core::ops::{DropVerb, drop_verb};
        if t.trash {
            return DropAction::Trash;
        }
        let Some(m) = &i.marker else {
            return if verb_is_move(i.can_copy, i.can_move) { DropAction::Move } else { DropAction::Copy };
        };
        let (ctrl, shift) = self.marker_keys(m);
        let same_disk = i.devices.get(&t.dest).copied().flatten().map(|d| d == m.device);
        match drop_verb(ctrl, shift, same_disk, m.can_delete) {
            DropVerb::Move => DropAction::Move,
            DropVerb::Copy => DropAction::Copy,
            DropVerb::Link => DropAction::Link,
        }
    }

    pub(crate) fn incoming_label(&self, i: &IncomingDrag, t: &DropTarget) -> String {
        let folder = t.dest.file_name().map_or_else(|| t.dest.to_string_lossy().into_owned(), |n| n.to_string_lossy().into_owned());
        match self.incoming_action(i, t) {
            DropAction::Trash => "Move to Trash".to_string(),
            DropAction::Move => format!("Move to {folder}"),
            DropAction::Copy => format!("Copy to {folder}"),
            DropAction::Link => format!("Link in {folder}"),
        }
    }

    fn refresh_incoming(&mut self) {
        let Some(mut i) = self.incoming.take() else { return };
        let target = self.incoming_target(&i);
        if target.as_ref().map(|t| t.hit) != i.target.as_ref().map(|t| t.hit) {
            for t in [i.target.as_ref(), target.as_ref()].into_iter().flatten() {
                self.invalidate_drop_target(t.hit);
            }
        }
        i.target = target;
        if let (Some(probe), Some(t)) = (&i.probe, &i.target)
            && !t.trash
            && !i.devices.contains_key(&t.dest)
        {
            i.devices.insert(t.dest.clone(), None);
            let _ = probe.send(t.dest.clone());
        }
        if i.accepting != i.target.is_some() {
            i.accepting = i.target.is_some();
            self.cmds.push(Cmd::AcceptDrop(i.accepting));
        }
        self.invalidate(i.label);
        i.label = match &i.target {
            Some(t) if i.moved => {
                let label = self.incoming_label(&i, t);
                self.drag_label_rect(&label, if i.marker.is_some() { GANNET_PICTURE } else { OTHER_APP_PICTURE })
            }
            _ => Rect::default(),
        };
        self.invalidate(i.label);
        self.incoming = Some(i);
    }

    fn drag_dropped(&mut self) {
        if let Some(FileDrag::Active(d)) = &mut self.file_drag
            && d.wayland
        {
            // Landing with no motion, nothing says where. Its own folder isn't a target anyway.
            if !d.moved_inside {
                if let Some(FileDrag::Active(d)) = self.file_drag.take() {
                    self.end_active_drag(&d);
                }
                self.cmds.push(Cmd::AbandonDrop);
                return;
            }
            // It's come back and landed here, so it ends like any other drag in this window.
            d.wayland = false;
            self.finish_file_drag();
            self.cmds.push(Cmd::FinishDrop);
            return;
        }
        let Some(i) = self.incoming.take() else {
            self.cmds.push(Cmd::AbandonDrop);
            return;
        };
        self.end_incoming(&i);
        // Looked up again: a reload can have moved the rows.
        match self.incoming_target(&i) {
            Some(t) => {
                let decided = i.marker.is_some().then(|| self.incoming_action(&i, &t));
                self.cmds.push(Cmd::ReadDrop { dest: t.dest, trash: t.trash, decided });
            }
            None => self.cmds.push(Cmd::AbandonDrop),
        }
    }

    /// Acts on a dropped file list. This window does the work, so progress, conflicts and
    /// undo are the same as anywhere else. Another Gannet window said what it knows, so the
    /// rule inside a window applies. A drop from another program copies unless that program
    /// only offers a move, because its keys and disks can't be seen from here.
    fn on_dropped(&mut self, d: Dropped) {
        if d.paths.is_empty() {
            // No files on this computer, or the other program stalled or sent too much.
            self.notice("Couldn't get any files from that drag".into());
            self.cmds.push(Cmd::AbandonDrop);
            return;
        }
        let request = if d.trash {
            if !d.can_move && d.decided.is_none() {
                self.notice("The other app only lets these be copied, so they can't go to the Trash".into());
                self.cmds.push(Cmd::AbandonDrop);
                return;
            }
            Request::Trash { paths: d.paths }
        } else if d.paths.iter().all(|p| p.parent() == Some(d.dest.as_path())) {
            // They're already here. Saying that worked could make a source delete them.
            self.cmds.push(Cmd::AbandonDrop);
            return;
        } else {
            let (sources, dest) = (d.paths, d.dest);
            match d.decided {
                Some(DropAction::Move) => Request::Move { sources, dest },
                Some(DropAction::Link) => Request::Link { sources, dest },
                Some(_) => Request::Copy { sources, dest },
                None if verb_is_move(d.can_copy, d.can_move) => Request::Move { sources, dest },
                None => Request::Copy { sources, dest },
            }
        };
        self.start_job(request);
        // Some compositors, Hyprland among them, refuse the pointer to every window until a drop
        // is finished, so waiting here could leave a question that can't be clicked. This window
        // only agreed to a copy (see the data device's Enter), and apps have to cope with hearing
        // that early, since GTK says it as soon as it has the file list. Only a source that
        // offered nothing but a move waits, as it may delete its files on hearing it. Another
        // Gannet window deletes nothing, whatever was agreed.
        if d.decided.is_none() && verb_is_move(d.can_copy, d.can_move) {
            self.drop_job = self.jobs.last().map(|j| (j.job.id, d.all_local));
        } else {
            self.cmds.push(if d.all_local { Cmd::FinishDrop } else { Cmd::AbandonDrop });
        }
    }

    /// Answers a source that only offered a move, once the files have really arrived. It may
    /// delete its side on hearing that, so a job that failed or stopped doesn't say so.
    fn answer_drop(&mut self, o: &ops::Outcome) {
        if let Some((job, all_local)) = self.drop_job
            && job == o.job
        {
            self.drop_job = None;
            self.cmds.push(if all_local && !o.cancelled && o.errors == 0 && o.skipped == 0 { Cmd::FinishDrop } else { Cmd::AbandonDrop });
        }
    }

    fn finish_file_drag(&mut self) {
        match self.file_drag.take() {
            Some(FileDrag::Pressed { row, dir, name, narrow: true, .. }) if self.pressed_row_is_current(row, &dir, &name) => self.move_cursor(row, Mods::default()),
            Some(FileDrag::Band(b)) => self.band_ended(&b),
            // A release means the compositor never took this drag: it stops those events.
            Some(FileDrag::Active(d)) if d.wayland => {
                self.cmds.push(Cmd::CancelDrag);
                self.end_active_drag(&d);
            }
            Some(FileDrag::Active(d)) => {
                // Looked up again: a reload during the drag can have moved the rows.
                let (x, y) = self.pointer;
                let target = self.drop_target_at(&d, x, y);
                self.end_active_drag(&d);
                let Some(t) = target else { return };
                let action = self.drop_action(&d, &t);
                let (sources, dest) = (d.paths, t.dest);
                self.start_job(match action {
                    DropAction::Move => Request::Move { sources, dest },
                    DropAction::Copy => Request::Copy { sources, dest },
                    DropAction::Link => Request::Link { sources, dest },
                    DropAction::Trash => Request::Trash { paths: sources },
                });
            }
            _ => {}
        }
    }
}

/// A folder's device, and whether its items can be deleted. In a sticky folder that isn't
/// ours, like `/tmp`, only some of them can, so it counts as no.
fn source_facts(dir: &Path) -> Option<(u64, bool)> {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::metadata(dir).ok()?;
    let writable = rustix::fs::access(dir, rustix::fs::Access::WRITE_OK).is_ok();
    let others_sticky = m.mode() & 0o1000 != 0 && m.uid() != rustix::process::geteuid().as_raw();
    Some((m.dev(), writable && !others_sticky))
}

/// The rows in `a` that aren't in `b`, as up to two runs.
fn rows_outside(a: Option<(usize, usize)>, b: Option<(usize, usize)>) -> [std::ops::Range<usize>; 2] {
    let Some((a0, a1)) = a else { return [0..0, 0..0] };
    let Some((b0, b1)) = b else { return [a0..a1 + 1, 0..0] };
    [a0..b0.min(a1 + 1), (b1 + 1).max(a0)..a1 + 1]
}

/// How far a drag's picture reaches below the pointer. Gannet draws a row. Other apps' pictures
/// can't be measured from here, and this clears Nautilus's 64 pixel stack of icons.
const GANNET_PICTURE: f64 = 44.0;
const OTHER_APP_PICTURE: f64 = 84.0;

/// What a drop from another program does. A move only if that's explicitly all it offers.
fn verb_is_move(can_copy: bool, can_move: bool) -> bool {
    can_move && !can_copy
}

fn cursor_for(h: Hit) -> Cursor {
    match h {
        Hit::Header(_) | Hit::Crumb(_) | Hit::Back | Hit::Forward | Hit::Up | Hit::EmptyTrash | Hit::Place(_) | Hit::MenuItem(_) | Hit::NoticeClose(_) | Hit::JobCancel(_) | Hit::DialogButton(_) | Hit::DialogCheck => Cursor::Pointer,
        Hit::PathBar | Hit::FilterBox => Cursor::Text,
        _ => Cursor::Default,
    }
}

fn next_enabled(items: &[MenuItem], from: usize, step: usize) -> usize {
    let n = items.len();
    let mut i = if from == usize::MAX { n - 1 } else { from.min(n - 1) };
    if from == n {
        i = 0;
    }
    for _ in 0..n {
        i = (i + step) % n;
        if items[i].enabled {
            return i;
        }
    }
    usize::MAX
}

fn title_for(dir: &Path) -> String {
    dir.file_name().map_or_else(|| "/".to_string(), |n| n.to_string_lossy().into_owned())
}

/// `~` and `~/x`, using the home the app was given rather than the environment.
pub fn expand_home(text: &str, home: Option<&Path>) -> PathBuf {
    let t = text.trim();
    match (t.strip_prefix('~'), home) {
        (Some(""), Some(h)) => h.to_path_buf(),
        (Some(rest), Some(h)) if rest.starts_with('/') => h.join(rest.trim_start_matches('/')),
        _ => PathBuf::from(t),
    }
}

/// Resolves `.` and `..` by text, the way a typed path is read.
pub fn clean(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// Kind and name to an icon.
pub fn icon_for(kind: Kind, name: &[u8]) -> crate::icons::Icon {
    use crate::icons::{Icon, icon_for_name};
    match kind {
        Kind::Dir => Icon::Folder,
        Kind::LinkDir => Icon::FolderLink,
        Kind::Link => Icon::FileLink,
        Kind::File | Kind::Other => icon_for_name(name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_paths_expand_and_clean() {
        let h = Some(Path::new("/home/me"));
        assert_eq!(expand_home("~", h), Path::new("/home/me"));
        assert_eq!(expand_home("~/x", h), Path::new("/home/me/x"));
        assert_eq!(expand_home("~bob", h), Path::new("~bob"));
        assert_eq!(expand_home(" /a ", h), Path::new("/a"));
        assert_eq!(clean(Path::new("/a/./b/../c/")), Path::new("/a/c"));
        assert_eq!(clean(Path::new("/../..")), Path::new("/"));
    }

    // ---- The app itself, driven directly, on scratch folders ----

    use gannet_core::trash::TrashConfig;
    use rustix::event::{EventfdFlags, eventfd};
    use std::fs;
    use std::sync::mpsc::{Receiver, channel};

    struct Rig {
        app: App,
        rx: Receiver<Msg>,
        root: PathBuf,
    }

    impl Rig {
        /// An app on a fresh scratch folder, its Trash in there too.
        fn new(tag: &str) -> Self {
            assert!(std::env::var_os("GANNET_TEST_SANDBOX").is_some(), "run tests with scripts/test.sh");
            let root = std::env::temp_dir().join(format!("gannet-test-app-{}-{tag}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(root.join("work")).unwrap();
            let (tx, rx) = channel();
            let hub = Hub { tx, wake: Arc::new(eventfd(0, EventfdFlags::CLOEXEC | EventfdFlags::NONBLOCK).unwrap()) };
            let op_hub = hub.clone();
            let mut trash = TrashConfig::new(&root.join("data"), 1000);
            trash.mount_of = Arc::new(|_| None);
            let engine = Engine::new(trash, Arc::new(move |e| op_hub.send(Msg::Op(e))));
            let mut app = App::new(hub, engine, root.join("data/Trash/files"), gannet_core::theme::builtin(false), Source::Default, Text::new(14.0, true), Vec::new(), root.join("work"), 1280, 800, 1.0);
            // Tests must never find a real disk's Trash.
            app.trash_mounts = Some(Vec::new());
            Self { app, rx, root }
        }

        fn work(&self) -> PathBuf {
            self.root.join("work")
        }

        fn pump_until(&mut self, what: &str, done: impl Fn(&App) -> bool) {
            let end = Instant::now() + Duration::from_secs(10);
            while !done(&self.app) {
                assert!(Instant::now() < end, "timed out: {what}");
                if let Ok(m) = self.rx.recv_timeout(Duration::from_millis(50)) {
                    self.app.on_msg(m);
                }
                self.app.on_timer(Instant::now());
            }
        }

        /// Opens `dir` and waits until it's fully read.
        fn open(&mut self, dir: PathBuf) {
            self.app.go(Stop { dir, cursor_name: None });
            self.pump_until("folder loaded", |a| !a.loading);
        }

        fn select(&mut self, name: &str) {
            let l = self.app.listing.clone().unwrap();
            let row = self.app.rows.iter().position(|&i| l.name(i) == name.as_bytes()).unwrap();
            self.app.move_cursor(row, Mods::default());
        }

        fn selected_names(&self) -> Vec<String> {
            let l = self.app.listing.clone().unwrap();
            self.app.selected_ids().into_iter().map(|i| String::from_utf8_lossy(l.name(i)).into_owned()).collect()
        }

        fn key(&mut self, sym: u32, utf8: &str) {
            self.app.key(sym, utf8, Mods::default());
        }

        fn ctrl(&mut self, sym: u32) {
            self.app.key(sym, "", Mods { ctrl: true, ..Mods::default() });
        }

        fn idle(&mut self) {
            self.pump_until("jobs done", |a| a.jobs.is_empty() && !a.loading);
        }
    }

    impl Drop for Rig {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn uri(p: &Path) -> Vec<u8> {
        format!("copy\nfile://{}", p.display()).into_bytes()
    }

    #[test]
    fn a_slow_clipboard_pastes_where_it_was_asked_not_where_you_are_now() {
        let mut r = Rig::new("paste-dest");
        fs::write(r.work().join("a"), "x").unwrap();
        for d in ["there", "here"] {
            fs::create_dir(r.root.join(d)).unwrap();
        }
        let (there, here) = (r.root.join("there"), r.root.join("here"));
        r.open(there.clone());
        r.app.action(Action::Paste);
        let Some(Cmd::ReadClipboard(asked)) = r.app.cmds.pop() else { panic!("no read requested") };
        // The user moves on before the other program answers.
        r.open(here.clone());
        let src = r.work().join("a");
        r.app.on_msg(Msg::Clipboard(Some(uri(&src)), clipboard::GNOME.to_string(), asked));
        r.idle();
        assert!(there.join("a").exists());
        assert!(!here.join("a").exists());
    }

    #[test]
    fn a_late_clipboard_answer_doesnt_land_in_another_field() {
        let mut r = Rig::new("paste-field");
        fs::write(r.work().join("a"), "x").unwrap();
        r.open(r.work());
        r.select("a");
        r.ctrl(k::KEY_l);
        r.ctrl(k::KEY_v);
        let Some(Cmd::ReadClipboard(asked)) = r.app.cmds.pop() else { panic!("no read requested") };
        r.key(k::KEY_Escape, "");
        r.key(k::KEY_F2, "");
        assert!(matches!(r.app.edit, Edit::Rename { .. }));
        r.app.on_msg(Msg::Clipboard(Some(b"junk".to_vec()), clipboard::TEXT.to_string(), asked));
        let Edit::Rename { field, .. } = &r.app.edit else { panic!() };
        assert_eq!(field.text, "a");
    }

    /// Starts a copy that will clash, and waits for its dialog.
    fn clash(r: &mut Rig, src_body: &str) -> (PathBuf, PathBuf) {
        let (src, dst) = (r.root.join("src-a"), r.work().join("src-a"));
        fs::write(&src, src_body).unwrap();
        fs::write(&dst, "old").unwrap();
        r.open(r.work());
        let dest = r.work();
        r.app.on_msg(Msg::Clipboard(Some(uri(&src)), clipboard::GNOME.to_string(), Paste::Files { dest }));
        r.pump_until("conflict shows", |a| a.dialog.is_some());
        (src, dst)
    }

    fn past_guard(r: &mut Rig) {
        r.app.dialog_guard = None;
    }

    #[test]
    fn enter_on_a_conflict_skips_rather_than_overwrites() {
        let mut r = Rig::new("conflict-enter");
        let (_, dst) = clash(&mut r, "new");
        past_guard(&mut r);
        r.key(k::KEY_Return, "\r");
        r.idle();
        assert_eq!(fs::read_to_string(dst).unwrap(), "old");
    }

    #[test]
    fn typing_doesnt_answer_a_conflict_that_just_appeared() {
        let mut r = Rig::new("conflict-guard");
        let (_, _) = clash(&mut r, "new");
        r.key(k::KEY_s, "s");
        assert!(r.app.dialog.is_some(), "a keystroke right as it appears must not answer it");
        past_guard(&mut r);
        r.key(k::KEY_s, "s");
        assert!(r.app.dialog.is_none());
    }

    #[test]
    fn no_letter_overwrites_on_a_conflict() {
        let mut r = Rig::new("conflict-letter");
        let (_, dst) = clash(&mut r, "new");
        past_guard(&mut r);
        r.key(k::KEY_r, "r");
        assert!(r.app.dialog.is_some(), "R must not replace");
        r.key(k::KEY_Left, "");
        r.key(k::KEY_Return, "\r");
        r.idle();
        assert_eq!(fs::read_to_string(dst).unwrap(), "new", "choosing Replace on purpose works");
    }

    #[test]
    fn entering_an_untouched_rename_leaves_a_bad_name_alone() {
        use std::os::unix::ffi::OsStrExt;
        let mut r = Rig::new("rename-bad");
        let raw = r.work().join(std::ffi::OsStr::from_bytes(b"caf\xe9.txt"));
        fs::write(&raw, "x").unwrap();
        r.open(r.work());
        r.app.move_cursor(0, Mods::default());
        r.key(k::KEY_F2, "");
        r.key(k::KEY_Return, "\r");
        r.idle();
        assert!(raw.exists(), "the original bytes must survive");
    }

    #[test]
    fn a_finishing_paste_selects_only_in_its_own_folder_and_respects_new_selections() {
        let mut r = Rig::new("select");
        fs::write(r.root.join("x"), "1").unwrap();
        fs::write(r.work().join("keep"), "").unwrap();
        let other = r.root.join("other");
        fs::create_dir(&other).unwrap();
        fs::write(other.join("x"), "same name, different file").unwrap();
        // Same folder: the user picks something while the paste runs.
        r.open(r.work());
        let dest = r.work();
        let src = r.root.join("x");
        r.app.on_msg(Msg::Clipboard(Some(uri(&src)), clipboard::GNOME.to_string(), Paste::Files { dest }));
        r.select("keep");
        r.idle();
        assert_eq!(r.selected_names(), ["keep"], "a selection made since must stay");
        // Another folder: a stale wish to select `x` must not apply there.
        r.app.pending_select = vec![b"x".to_vec()];
        r.open(other);
        assert!(r.selected_names().is_empty());
    }

    #[test]
    fn a_cut_only_moves_once() {
        let mut r = Rig::new("cut-once");
        fs::write(r.root.join("a"), "x").unwrap();
        r.open(r.work());
        let dest = r.work();
        let src = r.root.join("a");
        let data = format!("cut\nfile://{}", src.display()).into_bytes();
        r.app.on_msg(Msg::Clipboard(Some(data), clipboard::GNOME.to_string(), Paste::Files { dest }));
        assert!(r.app.cmds.iter().any(|c| matches!(c, Cmd::ClearClipboard)), "the clipboard must not still say cut");
        r.idle();
        assert!(r.work().join("a").exists());
    }

    #[test]
    fn two_quick_pastes_ask_only_once() {
        let mut r = Rig::new("paste-twice");
        r.open(r.work());
        r.ctrl(k::KEY_v);
        r.ctrl(k::KEY_v);
        assert_eq!(r.app.cmds.iter().filter(|c| matches!(c, Cmd::ReadClipboard(_))).count(), 1);
    }

    /// Puts the pointer on a row and presses, with `a` and `b` listed.
    fn press_on_row(r: &mut Rig, row: usize) {
        let l = r.app.layout();
        let y = l.list.y + row as i32 * l.row_h + l.row_h / 2;
        // On the row's icon, where a drag carries the file.
        r.app.input(Input::Move { x: (l.list.x + 20) as f64, y: y as f64 });
        r.app.input(Input::Button { button: BTN_LEFT, pressed: true });
    }

    fn drag_rig(tag: &str) -> Rig {
        let mut r = Rig::new(tag);
        fs::write(r.root.join("work/a"), b"1").unwrap();
        fs::write(r.root.join("work/b"), b"2").unwrap();
        r.app.input(Input::Key { keysym: k::KEY_F5, utf8: String::new(), mods: Mods::default(), repeat: false });
        r.pump_until("the files", |a| a.rows.len() == 2);
        r
    }

    #[test]
    fn a_press_survives_messages_and_keys_until_it_becomes_a_drag() {
        let mut r = drag_rig("drag-survive");
        press_on_row(&mut r, 0);
        r.app.input(Input::Mods(Mods { ctrl: true, ..Mods::default() }));
        r.app.input(Input::Mods(Mods::default()));
        r.app.on_msg(Msg::Notice("something finished".into()));
        assert!(matches!(r.app.file_drag, Some(FileDrag::Pressed { .. })));
        r.app.input(Input::Move { x: 540.0, y: 200.0 });
        assert!(matches!(r.app.file_drag, Some(FileDrag::Active(_))));
    }

    #[test]
    fn a_drag_ending_under_a_menu_puts_the_cursor_back() {
        let mut r = drag_rig("drag-cursor");
        press_on_row(&mut r, 0);
        r.app.input(Input::Move { x: 540.0, y: 200.0 });
        assert!(matches!(r.app.file_drag, Some(FileDrag::Active(_))));
        r.app.open_menu(800, 40);
        r.app.cmds.clear();
        r.app.input(Input::Key { keysym: k::KEY_Escape, utf8: String::new(), mods: Mods::default(), repeat: false });
        assert!(r.app.file_drag.is_none());
        assert!(r.app.cmds.iter().any(|c| matches!(c, Cmd::Cursor(Cursor::Default))), "the drag's cursor stayed");
    }

    fn dragging(tag: &str) -> Rig {
        let mut r = drag_rig(tag);
        press_on_row(&mut r, 0);
        r.app.input(Input::Move { x: 540.0, y: 500.0 });
        assert!(matches!(r.app.file_drag, Some(FileDrag::Active(_))));
        r
    }

    fn cursors(r: &App) -> Vec<Cursor> {
        r.cmds.iter().filter_map(|c| if let Cmd::Cursor(c) = c { Some(*c) } else { None }).collect()
    }

    #[test]
    fn a_menu_opening_cancels_a_drag_and_hands_back_the_pointer() {
        let mut r = dragging("drag-menu");
        // Away from the pointer, so nothing's under it.
        r.app.open_menu(800, 40);
        r.app.cmds.clear();
        r.app.on_msg(Msg::Notice("anything".into()));
        assert!(r.app.file_drag.is_none(), "the drag ends as soon as the menu is up");
        assert_eq!(cursors(&r.app), [Cursor::Default]);
        // The menu responds to the pointer now.
        let at = r.app.menu.as_ref().unwrap().at;
        let m = r.app.menu.as_ref().unwrap();
        let rect = r.app.menu_rect(m);
        let target = if at == 1 { 0 } else { 1 };
        let y = rect.y + r.app.px(4.0) + target as i32 * r.app.px(28.0) + r.app.px(14.0);
        r.app.input(Input::Move { x: (rect.x + r.app.px(10.0)) as f64, y: y as f64 });
        assert_eq!(r.app.menu.as_ref().unwrap().at, target);
    }

    #[test]
    fn a_dialog_opening_cancels_a_drag() {
        let mut r = dragging("drag-dialog");
        r.app.dialog = Some(Dialog::ConfirmQuit { at: 0 });
        r.app.on_msg(Msg::Notice("anything".into()));
        assert!(r.app.file_drag.is_none());
    }

    fn drags_started(a: &App) -> usize {
        a.cmds.iter().filter(|c| matches!(c, Cmd::StartDrag { .. })).count()
    }

    #[test]
    fn leaving_the_window_hands_the_drag_to_the_compositor() {
        let mut r = dragging("drag-leave");
        r.app.cmds.clear();
        r.app.input(Input::Leave);
        assert_eq!(drags_started(&r.app), 1);
        assert!(matches!(&r.app.file_drag, Some(FileDrag::Active(d)) if d.wayland));
        // The compositor takes the keyboard during its drags, and that doesn't end this one.
        r.app.input(Input::Focus(false));
        assert!(r.app.file_drag.is_some());
        // Nor does the leave that follows starting it, a second time.
        r.app.input(Input::Leave);
        assert_eq!(drags_started(&r.app), 1);
        r.app.cmds.clear();
        r.app.input(Input::OutDragEnded);
        assert!(r.app.file_drag.is_none());
        assert_eq!(cursors(&r.app), [Cursor::Default]);
        assert!(r.app.jobs.is_empty(), "the other program does the work");
    }

    #[test]
    fn moving_past_the_edge_hands_the_drag_over_once() {
        let mut r = dragging("drag-edge");
        r.app.cmds.clear();
        r.app.input(Input::Move { x: -5.0, y: 200.0 });
        assert_eq!(drags_started(&r.app), 1);
        let Cmd::StartDrag { formats, move_only, .. } = r.app.cmds.iter().find(|c| matches!(c, Cmd::StartDrag { .. })).unwrap() else { unreachable!() };
        assert!(!move_only, "a plain drag offers a copy");
        let list = &formats.iter().find(|(m, _)| *m == clipboard::URI_LIST).unwrap().1;
        assert!(String::from_utf8_lossy(list).contains("/work/a"));
        r.app.input(Input::Move { x: -9.0, y: 200.0 });
        assert_eq!(drags_started(&r.app), 1);
    }

    #[test]
    fn a_drag_out_stops_when_the_folder_changes() {
        let mut r = drag_rig("drag-out-folder");
        fs::create_dir(r.root.join("work/d")).unwrap();
        r.app.input(Input::Key { keysym: k::KEY_F5, utf8: String::new(), mods: Mods::default(), repeat: false });
        r.pump_until("the folder", |a| a.rows.len() == 3);
        press_on_row(&mut r, 1);
        r.app.input(Input::Move { x: -5.0, y: 200.0 });
        r.app.input(Input::Leave);
        assert!(matches!(&r.app.file_drag, Some(FileDrag::Active(d)) if d.wayland));
        r.app.cmds.clear();
        r.app.navigate(r.root.join("work/d"), None);
        r.app.input(Input::Mods(Mods::default()));
        assert!(r.app.file_drag.is_none());
        assert!(r.app.cmds.iter().any(|c| matches!(c, Cmd::CancelDrag)), "the compositor would still have it");
    }

    #[test]
    fn coming_back_in_a_wayland_drag_drops_like_any_other_drag() {
        let mut r = drag_rig("drag-back");
        fs::create_dir(r.root.join("work/d")).unwrap();
        r.app.input(Input::Key { keysym: k::KEY_F5, utf8: String::new(), mods: Mods::default(), repeat: false });
        r.pump_until("the folder", |a| a.rows.len() == 3);
        press_on_row(&mut r, 1);
        r.app.input(Input::Move { x: -5.0, y: 200.0 });
        r.app.input(Input::Leave);
        let l = r.app.layout();
        r.app.cmds.clear();
        let y = (l.list.y + l.row_h / 2) as f64;
        r.app.input(Input::DragEnter { x: 540.0, y, files: true, can_copy: true, can_move: true, marker: None });
        r.app.input(Input::DragMove { x: 540.0, y });
        assert!(r.app.cmds.iter().any(|c| matches!(c, Cmd::AcceptDrop(true))));
        // Until the disks have been looked at, a drop would copy.
        r.pump_until("the disk thread", |a| matches!(&a.file_drag, Some(FileDrag::Active(d)) if d.source.is_some() && d.devices.values().any(|v| v.is_some())));
        r.app.input(Input::DragDrop);
        assert!(r.app.cmds.iter().any(|c| matches!(c, Cmd::FinishDrop)));
        assert!(r.app.file_drag.is_none());
        r.pump_until("the move", |a| a.jobs.is_empty() && a.rows.len() == 2);
        assert!(r.root.join("work/d/a").exists() && !r.root.join("work/a").exists());
        assert!(r.app.incoming.is_none());
    }

    /// A folder of `n` files, which sort in row order.
    fn band_rig(tag: &str, n: usize) -> Rig {
        let mut r = Rig::new(tag);
        for i in 0..n {
            fs::write(r.root.join("work").join(format!("f{i:04}")), b"x").unwrap();
        }
        r.app.input(Input::Key { keysym: k::KEY_F5, utf8: String::new(), mods: Mods::default(), repeat: false });
        r.pump_until("the rows", |a| a.rows.len() == n && !a.loading);
        r
    }

    fn band_y(r: &Rig, row: usize) -> f64 {
        let l = r.app.layout();
        (l.list.y + row as i32 * l.row_h + l.row_h / 2) as f64 - r.app.scroll
    }

    fn band_x(r: &Rig) -> f64 {
        (r.app.layout().list.right() - 100) as f64
    }

    /// Presses beside the name of a row, or below the rows if there's no such row.
    fn band_press(r: &mut Rig, row: usize) {
        let (x, y) = (band_x(r), band_y(r, row));
        r.app.input(Input::Move { x, y });
        r.app.input(Input::Button { button: BTN_LEFT, pressed: true });
    }

    fn band_to(r: &mut Rig, row: usize) {
        let (x, y) = (band_x(r) - 40.0, band_y(r, row));
        r.app.input(Input::Move { x, y });
    }

    fn selected_rows(a: &App) -> Vec<usize> {
        (0..a.rows.len()).filter(|&i| a.is_selected(a.rows[i])).collect()
    }

    #[test]
    fn a_box_selects_what_it_touches_and_lets_go_of_what_it_leaves() {
        let mut r = band_rig("band", 8);
        band_press(&mut r, 1);
        band_to(&mut r, 3);
        assert_eq!(selected_rows(&r.app), [1, 2, 3]);
        assert_eq!(r.app.sel_count, 3);
        assert!(!r.app.selected_paths().is_empty());
        band_to(&mut r, 2);
        assert_eq!(selected_rows(&r.app), [1, 2]);
        // Back across the start, it works upwards.
        band_to(&mut r, 0);
        assert_eq!(selected_rows(&r.app), [0, 1]);
        r.app.input(Input::Button { button: BTN_LEFT, pressed: false });
        assert!(r.app.file_drag.is_none());
        assert_eq!(selected_rows(&r.app), [0, 1]);
        assert_eq!(r.app.sel_count, 2);
    }

    #[test]
    fn a_box_replaces_the_selection_and_escape_puts_it_back() {
        let mut r = band_rig("band-esc", 8);
        r.app.input(Input::Key { keysym: k::KEY_a, utf8: "a".into(), mods: Mods { ctrl: true, ..Mods::default() }, repeat: false });
        assert_eq!(r.app.sel_count, 8);
        // The row pressed is in the selection, and a drag from beside its name still draws a box.
        band_press(&mut r, 2);
        band_to(&mut r, 3);
        assert_eq!(selected_rows(&r.app), [2, 3]);
        r.app.input(Input::Key { keysym: k::KEY_Escape, utf8: String::new(), mods: Mods::default(), repeat: false });
        assert!(r.app.file_drag.is_none());
        assert_eq!(r.app.sel_count, 8, "as it was before the press");
        // The rest of the drag does nothing.
        band_to(&mut r, 5);
        r.app.input(Input::Button { button: BTN_LEFT, pressed: false });
        assert_eq!(r.app.sel_count, 8);
    }

    #[test]
    fn a_box_with_ctrl_adds_and_gives_back_rows_it_leaves() {
        let mut r = band_rig("band-ctrl", 8);
        r.app.input(Input::Mods(Mods { ctrl: true, ..Mods::default() }));
        for row in [1, 5] {
            band_press(&mut r, row);
            r.app.input(Input::Button { button: BTN_LEFT, pressed: false });
        }
        assert_eq!(selected_rows(&r.app), [1, 5]);
        std::thread::sleep(DOUBLE_CLICK);
        band_press(&mut r, 3);
        band_to(&mut r, 5);
        assert_eq!(selected_rows(&r.app), [1, 3, 4, 5]);
        band_to(&mut r, 4);
        assert_eq!(selected_rows(&r.app), [1, 3, 4, 5], "row 5 was selected before");
        band_to(&mut r, 3);
        assert_eq!(selected_rows(&r.app), [1, 3, 5]);
        r.app.input(Input::Button { button: BTN_LEFT, pressed: false });
        assert_eq!(r.app.sel_count, 3);
    }

    #[test]
    fn a_box_from_below_the_rows_touches_nothing_until_it_reaches_one() {
        let mut r = band_rig("band-below", 3);
        r.app.input(Input::Key { keysym: k::KEY_a, utf8: "a".into(), mods: Mods { ctrl: true, ..Mods::default() }, repeat: false });
        band_press(&mut r, 6);
        assert_eq!(r.app.sel_count, 0, "a press there clears, as it always has");
        band_to(&mut r, 5);
        assert_eq!(r.app.sel_count, 0);
        band_to(&mut r, 2);
        assert_eq!(selected_rows(&r.app), [2]);
        band_to(&mut r, 9);
        assert_eq!(r.app.sel_count, 0);
        r.app.input(Input::Button { button: BTN_LEFT, pressed: false });
    }

    #[test]
    fn a_press_beside_a_name_that_doesnt_move_is_a_click() {
        let mut r = band_rig("band-click", 8);
        r.app.input(Input::Key { keysym: k::KEY_a, utf8: "a".into(), mods: Mods { ctrl: true, ..Mods::default() }, repeat: false });
        band_press(&mut r, 2);
        assert_eq!(r.app.sel_count, 8, "not until the release, as it might have been a drag");
        r.app.input(Input::Button { button: BTN_LEFT, pressed: false });
        assert_eq!(selected_rows(&r.app), [2]);
    }

    #[test]
    fn a_small_wobble_beside_a_name_isnt_a_box() {
        let mut r = band_rig("band-wobble", 8);
        band_press(&mut r, 2);
        let (x, y) = (band_x(&r) + 1.0, band_y(&r, 2) + 1.0);
        r.app.input(Input::Move { x, y });
        assert!(matches!(r.app.file_drag, Some(FileDrag::Pressed { .. })));
        r.app.input(Input::Button { button: BTN_LEFT, pressed: false });
        assert_eq!(selected_rows(&r.app), [2]);
    }

    #[test]
    fn a_press_on_the_icon_or_name_still_carries_the_file() {
        let mut r = band_rig("band-name", 8);
        press_on_row(&mut r, 2);
        let l = r.app.layout();
        r.app.input(Input::Move { x: (l.list.x + 60) as f64, y: band_y(&r, 4) });
        assert!(matches!(r.app.file_drag, Some(FileDrag::Active(_))));
        r.app.input(Input::Key { keysym: k::KEY_Escape, utf8: String::new(), mods: Mods::default(), repeat: false });
    }

    #[test]
    fn a_box_scrolls_past_the_end_and_keeps_its_start() {
        let mut r = band_rig("band-scroll", 300);
        band_press(&mut r, 2);
        band_to(&mut r, 4);
        let l = r.app.layout();
        r.app.input(Input::Move { x: band_x(&r), y: (l.list.bottom() + 40) as f64 });
        assert!(r.app.next_deadline().is_some(), "it has further to go");
        for _ in 0..60 {
            let due = r.app.next_deadline().unwrap();
            r.app.on_timer(due);
        }
        assert!(r.app.scroll > 0.0);
        let rows = selected_rows(&r.app);
        assert_eq!(rows[0], 2, "the start scrolled off and still counts");
        assert_eq!(rows.len(), rows[rows.len() - 1] - 1, "with nothing skipped");
        assert!(rows.len() > 30);
        // Coming back into the list stops it, so an idle window doesn't keep waking.
        r.app.input(Input::Move { x: band_x(&r), y: (l.list.y + 100) as f64 });
        let (scroll, n) = (r.app.scroll, r.app.sel_count);
        r.app.on_timer(Instant::now() + Duration::from_secs(1));
        assert_eq!((r.app.scroll, r.app.sel_count), (scroll, n));
        r.app.input(Input::Button { button: BTN_LEFT, pressed: false });
        assert_eq!(r.app.sel_count, n);
        // Upwards as far as it goes, where the waking stops too.
        std::thread::sleep(DOUBLE_CLICK);
        let row = (r.app.scroll / l.row_h as f64) as usize + 3;
        band_press(&mut r, row);
        band_to(&mut r, row - 1);
        r.app.input(Input::Move { x: band_x(&r), y: (l.list.y - 40) as f64 });
        let mut ticks = 0;
        while let Some(due) = r.app.next_deadline() {
            r.app.on_timer(due);
            ticks += 1;
            assert!(ticks < 2000, "it never stopped");
        }
        assert_eq!(r.app.scroll, 0.0);
        assert_eq!(selected_rows(&r.app)[0], 0);
    }

    #[test]
    fn a_wheel_during_a_box_keeps_the_selection_up_to_date() {
        let mut r = band_rig("band-wheel", 300);
        band_press(&mut r, 2);
        band_to(&mut r, 4);
        r.app.input(Input::Scroll { lines: 20.0, pixels: 0.0 });
        assert!(r.app.scroll > 0.0);
        let rows = selected_rows(&r.app);
        assert_eq!(rows[0], 2);
        assert!(rows.len() > 10, "{}", rows.len());
    }

    #[test]
    fn a_reload_or_a_dialog_ends_a_box_and_keeps_what_it_selected() {
        let mut r = band_rig("band-reload", 8);
        band_press(&mut r, 1);
        band_to(&mut r, 3);
        fs::write(r.root.join("work/new"), b"x").unwrap();
        r.app.reload();
        r.pump_until("the new row", |a| a.rows.len() == 9 && !a.loading);
        assert!(r.app.file_drag.is_none());
        assert_eq!(r.app.sel_count, 3);
        band_press(&mut r, 5);
        band_to(&mut r, 6);
        r.app.dialog = Some(Dialog::ConfirmQuit { at: 0 });
        r.app.input(Input::Mods(Mods::default()));
        assert!(r.app.file_drag.is_none());
        assert_eq!(r.app.sel_count, 2);
    }

    #[test]
    fn new_rows_arriving_end_a_box_rather_than_leave_it_on_rows_that_have_gone() {
        let mut r = band_rig("band-derived", 8);
        band_press(&mut r, 3);
        band_to(&mut r, 7);
        let fewer: Vec<u32> = r.app.rows[..2].to_vec();
        r.app.apply_rows(fewer);
        assert!(r.app.file_drag.is_none());
        // It would have looked up rows 3 to 7, which aren't there now.
        band_to(&mut r, 1);
        assert!(r.app.file_drag.is_none());
    }

    #[test]
    fn the_same_rows_arriving_leave_a_box_alone() {
        let mut r = band_rig("band-same", 8);
        band_press(&mut r, 3);
        band_to(&mut r, 5);
        let same = r.app.rows.clone();
        r.app.apply_rows(same);
        band_to(&mut r, 6);
        assert_eq!(selected_rows(&r.app), [3, 4, 5, 6]);
    }

    #[test]
    fn a_folder_that_fails_to_load_ends_a_box() {
        let mut r = band_rig("band-failed", 8);
        band_press(&mut r, 3);
        band_to(&mut r, 7);
        let generation = r.app.load_gen;
        let dir = r.root.join("work");
        r.app.on_msg(Msg::Load(LoadEvent::Failed { generation, dir, error: "gone".into() }));
        assert!(r.app.file_drag.is_none());
        assert!(r.app.rows.is_empty());
        band_to(&mut r, 1);
        r.app.input(Input::Button { button: BTN_LEFT, pressed: false });
    }

    #[test]
    fn a_press_beside_a_name_doesnt_become_a_box_once_the_rows_change() {
        let mut r = band_rig("band-stale", 8);
        r.app.input(Input::Mods(Mods { ctrl: true, ..Mods::default() }));
        band_press(&mut r, 3);
        // Reordered, with the pressed row where it was, so it still looks current.
        let mut moved = r.app.rows.clone();
        moved.swap(0, 1);
        r.app.apply_rows(moved);
        assert!(r.app.file_drag.is_none());
        let n = r.app.sel_count;
        band_to(&mut r, 6);
        assert!(r.app.file_drag.is_none());
        assert_eq!(r.app.sel_count, n);
    }

    #[test]
    fn going_to_another_folder_ends_a_box() {
        let mut r = band_rig("band-go", 8);
        band_press(&mut r, 3);
        band_to(&mut r, 7);
        let dir = r.root.clone();
        r.app.navigate(dir, None);
        assert!(r.app.file_drag.is_none());
        band_to(&mut r, 1);
        assert!(r.app.file_drag.is_none());
    }

    #[test]
    fn a_box_starts_on_the_row_pressed_even_if_the_list_scrolled_before_it_moved() {
        let mut r = band_rig("band-wheel-first", 300);
        band_press(&mut r, 5);
        r.app.input(Input::Scroll { lines: 1.0, pixels: 0.0 });
        assert!(r.app.scroll > 0.0);
        band_to(&mut r, 10);
        assert_eq!(selected_rows(&r.app), [5, 6, 7, 8, 9, 10]);
    }

    #[test]
    fn a_box_touching_nothing_again_doesnt_leave_a_cursor_behind() {
        let mut r = band_rig("band-cursor", 3);
        band_press(&mut r, 6);
        band_to(&mut r, 2);
        assert_eq!(r.app.cursor, 2);
        assert!(r.app.cursor_shown);
        band_to(&mut r, 6);
        assert_eq!(r.app.sel_count, 0);
        assert!(!r.app.cursor_shown);
    }

    #[test]
    fn a_drag_arriving_ends_a_box_and_its_scrolling() {
        let mut r = band_rig("band-incoming", 300);
        band_press(&mut r, 2);
        band_to(&mut r, 4);
        let l = r.app.layout();
        r.app.input(Input::Move { x: band_x(&r), y: (l.list.bottom() + 40) as f64 });
        assert!(r.app.next_deadline().is_some());
        r.app.input(Input::DragEnter { x: 540.0, y: 200.0, files: true, can_copy: true, can_move: true, marker: None });
        assert!(r.app.file_drag.is_none());
        assert_eq!(r.app.next_deadline(), None);
        r.app.input(Input::DragLeave);
        // The same goes for a press that hasn't become anything yet.
        band_press(&mut r, 2);
        r.app.input(Input::DragEnter { x: 540.0, y: 200.0, files: true, can_copy: true, can_move: true, marker: None });
        assert!(r.app.file_drag.is_none());
    }

    #[test]
    fn a_box_paints_only_while_it_lasts() {
        let mut r = band_rig("band-rect", 8);
        band_press(&mut r, 1);
        band_to(&mut r, 3);
        let Some(FileDrag::Band(b)) = &r.app.file_drag else { panic!("no box") };
        let (rect, l) = (b.rect, r.app.layout());
        assert!(!rect.is_empty() && l.list.intersect(rect) == rect, "{rect:?}");
        // Past the end of the window, it stays in the list.
        r.app.input(Input::Move { x: -50.0, y: -50.0 });
        let Some(FileDrag::Band(b)) = &r.app.file_drag else { panic!("no box") };
        let last = b.rect;
        assert!(l.list.intersect(last) == last, "{last:?}");
        r.app.dirty = Rect::default();
        r.app.input(Input::Button { button: BTN_LEFT, pressed: false });
        assert!(r.app.dirty.intersects(last), "the box gets painted over");
    }

    fn dropped(r: &Rig, names: &[&str], can_copy: bool, can_move: bool) -> Dropped {
        Dropped { paths: names.iter().map(|n| r.root.join(n)).collect(), dest: r.root.join("work"), trash: false, can_copy, can_move, decided: None, all_local: true }
    }

    fn outside_files(r: &Rig) {
        fs::create_dir_all(r.root.join("outside")).unwrap();
        fs::write(r.root.join("outside/x"), b"x").unwrap();
    }

    fn answered(a: &App) -> (bool, bool) {
        (a.cmds.iter().any(|c| matches!(c, Cmd::FinishDrop)), a.cmds.iter().any(|c| matches!(c, Cmd::AbandonDrop)))
    }

    /// Hovering a drag from another Gannet window, with the folder's device known.
    fn marked_drag(tag: &str, marker: clipboard::DragMarker, device: Option<u64>) -> Rig {
        let mut r = drag_rig(tag);
        r.app.input(Input::DragEnter { x: 540.0, y: 500.0, files: true, can_copy: true, can_move: false, marker: Some(marker) });
        r.app.input(Input::DragMove { x: 540.0, y: 500.0 });
        let i = r.app.incoming.as_ref().unwrap();
        assert!(i.devices.contains_key(&r.root.join("work")), "it asked about this folder");
        let (drag, path) = (i.id, r.root.join("work"));
        r.app.on_msg(Msg::DragDevice { drag, path, device });
        r
    }

    fn incoming_verb(r: &Rig) -> DropAction {
        let i = r.app.incoming.as_ref().unwrap();
        r.app.incoming_action(i, i.target.as_ref().unwrap())
    }

    fn marker(can_delete: bool, ctrl: bool, shift: bool) -> clipboard::DragMarker {
        clipboard::DragMarker { device: 7, can_delete, ctrl, shift }
    }

    #[test]
    fn a_drag_from_another_gannet_window_follows_the_rule_inside_a_window() {
        for (tag, m, device, want) in [
            ("mark-same", marker(true, false, false), Some(7), DropAction::Move),
            ("mark-other", marker(true, false, false), Some(8), DropAction::Copy),
            ("mark-unknown", marker(true, false, false), None, DropAction::Copy),
            ("mark-undeletable", marker(false, false, false), Some(7), DropAction::Copy),
            ("mark-ctrl", marker(true, true, false), Some(7), DropAction::Copy),
            ("mark-shift", marker(true, false, true), Some(8), DropAction::Move),
            ("mark-both", marker(true, true, true), Some(7), DropAction::Link),
        ] {
            let r = marked_drag(tag, m, device);
            assert_eq!(incoming_verb(&r), want, "{tag}");
        }
    }

    /// A rig whose first row is a folder, and the middle of that row.
    fn rig_with_a_folder(tag: &str) -> (Rig, f64) {
        let mut r = drag_rig(tag);
        fs::create_dir(r.root.join("work/d")).unwrap();
        r.app.input(Input::Key { keysym: k::KEY_F5, utf8: String::new(), mods: Mods::default(), repeat: false });
        r.pump_until("the folder", |a| a.rows.len() == 3);
        let l = r.app.layout();
        (r, (l.list.y + l.row_h / 2) as f64)
    }

    #[test]
    fn a_drags_first_position_isnt_believed() {
        // Hyprland says the middle of the window, here a folder's row, before the real position.
        let (mut r, on_folder) = rig_with_a_folder("in-first-pos");
        r.app.cmds.clear();
        r.app.input(Input::DragEnter { x: 540.0, y: on_folder, files: true, can_copy: true, can_move: true, marker: None });
        let i = r.app.incoming.as_ref().unwrap();
        assert!(i.label.is_empty(), "no label yet");
        assert_eq!(i.target.as_ref().unwrap().hit, Hit::None, "nothing lights up");
        assert!(r.app.cmds.iter().any(|c| matches!(c, Cmd::AcceptDrop(true))), "a drop right away still works");
        r.app.input(Input::DragMove { x: 540.0, y: 500.0 });
        let i = r.app.incoming.as_ref().unwrap();
        assert_eq!(i.target.as_ref().unwrap().hit, Hit::ListEmpty);
        assert!(!i.label.is_empty());
        assert!((i.label.y - 500).abs() < r.app.px(100.0), "the label is by the pointer: {:?}", i.label);
    }

    #[test]
    fn a_drop_before_any_motion_goes_to_the_folder_showing() {
        let (mut r, on_folder) = rig_with_a_folder("in-first-drop");
        r.app.input(Input::DragEnter { x: 540.0, y: on_folder, files: true, can_copy: true, can_move: true, marker: None });
        r.app.cmds.clear();
        r.app.input(Input::DragDrop);
        let want = r.root.join("work");
        assert!(r.app.cmds.iter().any(|c| matches!(c, Cmd::ReadDrop { dest, trash: false, .. } if *dest == want)));
    }

    #[test]
    fn a_drag_coming_back_waits_for_its_first_motion() {
        let (mut r, on_folder) = rig_with_a_folder("in-first-back");
        press_on_row(&mut r, 1);
        r.app.input(Input::Move { x: -5.0, y: 200.0 });
        r.app.input(Input::Leave);
        r.app.cmds.clear();
        r.app.input(Input::DragEnter { x: 540.0, y: on_folder, files: true, can_copy: true, can_move: true, marker: None });
        let Some(FileDrag::Active(d)) = &r.app.file_drag else { panic!("no drag") };
        assert!(d.target.is_none() && d.label.is_empty(), "nothing lights up");
        assert!(!r.app.cmds.iter().any(|c| matches!(c, Cmd::AcceptDrop(true))));
        r.app.input(Input::DragMove { x: 540.0, y: on_folder });
        let Some(FileDrag::Active(d)) = &r.app.file_drag else { panic!("no drag") };
        assert!(d.target.is_some() && !d.label.is_empty());
        assert!(r.app.cmds.iter().any(|c| matches!(c, Cmd::AcceptDrop(true))));
    }

    #[test]
    fn a_drag_landing_back_before_any_motion_does_nothing() {
        let (mut r, on_folder) = rig_with_a_folder("in-first-back-drop");
        press_on_row(&mut r, 1);
        r.app.input(Input::Move { x: -5.0, y: 200.0 });
        r.app.input(Input::Leave);
        r.app.input(Input::DragEnter { x: 540.0, y: on_folder, files: true, can_copy: true, can_move: true, marker: None });
        r.app.input(Input::OutDragDropped);
        assert!(r.app.file_drag.is_some(), "the drop is on its way");
        r.app.cmds.clear();
        r.app.input(Input::DragDrop);
        assert!(r.app.file_drag.is_none() && r.app.jobs.is_empty());
        assert_eq!(answered(&r.app), (false, true));
        assert_eq!(r.app.hover, Hit::None, "nothing under that first position lights up");
        assert!(r.root.join("work/a").exists() && !r.root.join("work/d/a").exists());
    }

    #[test]
    fn leaving_and_coming_back_needs_a_new_motion() {
        let (mut r, on_folder) = rig_with_a_folder("in-first-back-again");
        press_on_row(&mut r, 1);
        r.app.input(Input::Move { x: -5.0, y: 200.0 });
        r.app.input(Input::Leave);
        r.app.input(Input::DragEnter { x: 540.0, y: 500.0, files: true, can_copy: true, can_move: true, marker: None });
        r.app.input(Input::DragMove { x: 540.0, y: 500.0 });
        r.app.input(Input::DragLeave);
        r.app.input(Input::DragEnter { x: 540.0, y: on_folder, files: true, can_copy: true, can_move: true, marker: None });
        let Some(FileDrag::Active(d)) = &r.app.file_drag else { panic!("no drag") };
        assert!(d.inside && !d.moved_inside && d.target.is_none() && d.label.is_empty());
    }

    #[test]
    fn a_marked_drag_uses_live_keys_only_with_the_keyboard() {
        let mut r = marked_drag("mark-keys", marker(true, true, false), Some(7));
        assert_eq!(incoming_verb(&r), DropAction::Copy, "without the keyboard, the keys at the lift count");
        r.app.input(Input::Focus(true));
        assert_eq!(incoming_verb(&r), DropAction::Move, "with it, what's held now counts: nothing");
        r.app.input(Input::Mods(Mods { ctrl: true, ..Mods::default() }));
        assert_eq!(incoming_verb(&r), DropAction::Copy);
        r.app.input(Input::Mods(Mods { ctrl: true, shift: true, ..Mods::default() }));
        assert_eq!(incoming_verb(&r), DropAction::Link);
        r.app.input(Input::Focus(false));
        assert_eq!(incoming_verb(&r), DropAction::Copy, "losing it goes back to the lift");
    }

    #[test]
    fn the_label_says_what_a_marked_drag_does() {
        let label = |r: &Rig| {
            let i = r.app.incoming.as_ref().unwrap();
            r.app.incoming_label(i, i.target.as_ref().unwrap())
        };
        assert_eq!(label(&marked_drag("mark-label-move", marker(true, false, false), Some(7))), "Move to work");
        assert_eq!(label(&marked_drag("mark-label-copy", marker(true, false, false), Some(8))), "Copy to work");
        assert_eq!(label(&marked_drag("mark-label-link", marker(true, true, true), Some(7))), "Link in work");
    }

    #[test]
    fn the_verb_of_a_marked_drag_reaches_the_drop() {
        let mut r = marked_drag("mark-drop", marker(true, false, false), Some(7));
        r.app.cmds.clear();
        r.app.input(Input::DragDrop);
        assert!(r.app.cmds.iter().any(|c| matches!(c, Cmd::ReadDrop { decided: Some(DropAction::Move), .. })));
    }

    #[test]
    fn a_marked_move_moves_though_the_offer_only_says_copy() {
        let mut r = drag_rig("mark-move");
        outside_files(&r);
        let mut d = dropped(&r, &["outside/x"], true, false);
        d.decided = Some(DropAction::Move);
        r.app.on_msg(Msg::Dropped(d));
        r.pump_until("the move", |a| a.jobs.is_empty() && a.rows.len() == 3);
        assert!(r.root.join("work/x").exists() && !r.root.join("outside/x").exists());
        assert_eq!(answered(&r.app), (true, false));
    }

    #[test]
    fn a_marked_link_links() {
        let mut r = drag_rig("mark-link");
        outside_files(&r);
        let mut d = dropped(&r, &["outside/x"], true, false);
        d.decided = Some(DropAction::Link);
        r.app.on_msg(Msg::Dropped(d));
        r.pump_until("the link", |a| a.jobs.is_empty() && a.rows.len() == 3);
        assert!(fs::symlink_metadata(r.root.join("work/x")).unwrap().file_type().is_symlink());
        assert!(r.root.join("outside/x").exists());
    }

    #[test]
    fn another_app_cant_cause_a_move_by_allowing_both() {
        let mut r = drag_rig("mark-foreign");
        r.app.input(Input::DragEnter { x: 540.0, y: 500.0, files: true, can_copy: true, can_move: true, marker: None });
        assert_eq!(incoming_verb(&r), DropAction::Copy);
        r.app.cmds.clear();
        r.app.input(Input::DragDrop);
        assert!(r.app.cmds.iter().any(|c| matches!(c, Cmd::ReadDrop { decided: None, .. })));
    }

    #[test]
    fn a_drag_out_carries_the_marker_once_its_folder_has_answered() {
        let started = |tag: &str, facts: Option<(u64, bool)>, mods: Mods| {
            let mut r = dragging(tag);
            let Some(FileDrag::Active(d)) = &mut r.app.file_drag else { panic!("no drag") };
            d.source = facts;
            r.app.input(Input::Mods(mods));
            r.app.cmds.clear();
            r.app.input(Input::Leave);
            r.app.cmds.iter().find_map(|c| if let Cmd::StartDrag { marker, .. } = c { Some(*marker) } else { None }).unwrap()
        };
        assert_eq!(started("out-mark", Some((7, true)), Mods::default()), Some(marker(true, false, false)));
        assert_eq!(started("out-mark-keys", Some((7, false)), Mods { ctrl: true, shift: true, ..Mods::default() }), Some(marker(false, true, true)));
        assert_eq!(started("out-mark-none", None, Mods::default()), None, "it just copies");
    }

    #[test]
    fn a_drop_from_another_app_copies_when_it_allows_that() {
        let mut r = drag_rig("in-copy");
        outside_files(&r);
        r.app.on_msg(Msg::Dropped(dropped(&r, &["outside/x"], true, true)));
        assert_eq!(answered(&r.app), (true, false), "at once, as some compositors hold the pointer until then");
        r.pump_until("the copy", |a| a.jobs.is_empty() && a.rows.len() == 3);
        assert!(r.root.join("work/x").exists() && r.root.join("outside/x").exists(), "the original stays");
    }

    #[test]
    fn a_question_about_a_copied_drop_doesnt_hold_the_drop_open() {
        let mut r = drag_rig("in-question");
        outside_files(&r);
        fs::write(r.root.join("work/x"), b"already").unwrap();
        r.app.on_msg(Msg::Dropped(dropped(&r, &["outside/x"], true, true)));
        r.pump_until("the question", |a| a.dialog.is_some());
        assert_eq!(answered(&r.app), (true, false), "or the question couldn't be clicked");
    }

    #[test]
    fn a_marked_drop_doesnt_hold_the_drop_open_even_for_a_move() {
        let mut r = drag_rig("in-marked-question");
        outside_files(&r);
        fs::write(r.root.join("work/x"), b"already").unwrap();
        let mut d = dropped(&r, &["outside/x"], false, true);
        d.decided = Some(DropAction::Move);
        r.app.on_msg(Msg::Dropped(d));
        r.pump_until("the question", |a| a.dialog.is_some());
        assert_eq!(answered(&r.app), (true, false), "another Gannet window deletes nothing on hearing it");
    }

    #[test]
    fn a_move_only_source_waits_until_the_files_have_arrived() {
        let mut r = drag_rig("in-move-waits");
        outside_files(&r);
        r.app.on_msg(Msg::Dropped(dropped(&r, &["outside/x"], false, true)));
        assert_eq!(answered(&r.app), (false, false), "it may delete its files on hearing it");
        r.pump_until("the move", |a| a.jobs.is_empty());
        assert_eq!(answered(&r.app), (true, false));
    }

    #[test]
    fn another_apps_label_clears_its_picture() {
        let mut r = drag_rig("in-label");
        let l = r.app.layout();
        let y = l.list.y + l.row_h / 2;
        r.app.input(Input::DragEnter { x: 540.0, y: y as f64, files: true, can_copy: true, can_move: true, marker: None });
        r.app.input(Input::DragMove { x: 540.0, y: y as f64 });
        let label = r.app.incoming.as_ref().unwrap().label;
        assert!(label.y >= y + 64, "Nautilus draws 64 pixels down from the pointer: {} from {y}", label.y);
        // With no room below, it goes above the pointer instead of being pushed under the picture.
        let y = r.app.h - 10;
        r.app.input(Input::DragMove { x: 540.0, y: y as f64 });
        let label = r.app.incoming.as_ref().unwrap().label;
        assert!(label.bottom() <= y, "{} from {y}", label.bottom());
    }

    #[test]
    fn an_app_that_only_offers_a_move_gets_a_move() {
        let mut r = drag_rig("in-move");
        outside_files(&r);
        r.app.on_msg(Msg::Dropped(dropped(&r, &["outside/x"], false, true)));
        r.pump_until("the move", |a| a.jobs.is_empty() && a.rows.len() == 3);
        assert!(r.root.join("work/x").exists() && !r.root.join("outside/x").exists());
        assert_eq!(answered(&r.app), (true, false));
    }

    #[test]
    fn a_failed_drop_doesnt_tell_the_other_app_it_worked() {
        let mut r = drag_rig("in-failed");
        r.app.on_msg(Msg::Dropped(dropped(&r, &["outside/missing"], false, true)));
        r.pump_until("the job", |a| a.jobs.is_empty());
        assert_eq!(answered(&r.app), (false, true), "so it keeps its files");
    }

    #[test]
    fn a_source_offering_neither_gets_a_copy_not_a_move() {
        let mut r = drag_rig("in-neither");
        outside_files(&r);
        r.app.on_msg(Msg::Dropped(dropped(&r, &["outside/x"], false, false)));
        r.pump_until("the copy", |a| a.jobs.is_empty() && a.rows.len() == 3);
        assert!(r.root.join("outside/x").exists());
    }

    #[test]
    fn a_skipped_item_isnt_reported_as_arrived() {
        let mut r = drag_rig("in-skip");
        outside_files(&r);
        fs::write(r.root.join("work/x"), b"already").unwrap();
        r.app.on_msg(Msg::Dropped(dropped(&r, &["outside/x"], false, true)));
        r.pump_until("the question", |a| a.dialog.is_some());
        std::thread::sleep(DIALOG_GUARD + Duration::from_millis(50));
        r.app.input(Input::Key { keysym: k::KEY_s, utf8: "s".into(), mods: Mods::default(), repeat: false });
        r.pump_until("the job", |a| a.jobs.is_empty());
        assert_eq!(answered(&r.app), (false, true), "so a move-only source keeps its file");
        assert!(r.root.join("outside/x").exists());
        assert_eq!(fs::read(r.root.join("work/x")).unwrap(), b"already");
    }

    #[test]
    fn a_partly_remote_list_isnt_reported_as_arrived() {
        let mut r = drag_rig("in-remote");
        outside_files(&r);
        for (can_copy, can_move) in [(false, true), (true, true)] {
            let mut d = dropped(&r, &["outside/x"], can_copy, can_move);
            d.all_local = false;
            r.app.cmds.clear();
            r.app.on_msg(Msg::Dropped(d));
            r.pump_until("the job", |a| a.jobs.is_empty());
            assert_eq!(answered(&r.app), (false, true), "so the source keeps the remote entry");
            let _ = fs::remove_file(r.root.join("work/x"));
            r.pump_until("the folder", |a| a.rows.len() == 2);
        }
    }

    #[test]
    fn a_drop_while_another_is_handled_is_refused() {
        let mut r = drag_rig("in-busy");
        r.app.input(Input::DragEnter { x: 540.0, y: 500.0, files: true, can_copy: true, can_move: true, marker: None });
        assert!(r.app.incoming.is_some());
        r.app.input(Input::DragDropBusy);
        assert!(r.app.incoming.is_none() && r.app.jobs.is_empty());
        assert!(r.app.notices.iter().any(|n| n.contains("still being dealt")));
    }

    #[test]
    fn leaving_from_over_a_button_doesnt_leave_its_cursor_behind() {
        let mut r = drag_rig("out-header");
        press_on_row(&mut r, 0);
        let l = r.app.layout();
        r.app.input(Input::Move { x: 300.0, y: (l.header.y + l.header.h / 2) as f64 });
        assert!(matches!(r.app.file_drag, Some(FileDrag::Active(_))));
        r.app.input(Input::Leave);
        r.app.cmds.clear();
        r.app.input(Input::OutDragEnded);
        assert_eq!(cursors(&r.app), [Cursor::Default]);
        assert_eq!(r.app.hover, Hit::None);
    }

    #[test]
    fn a_drop_landing_here_isnt_ended_by_the_sources_event() {
        let mut r = drag_rig("out-order");
        fs::create_dir(r.root.join("work/d")).unwrap();
        r.app.input(Input::Key { keysym: k::KEY_F5, utf8: String::new(), mods: Mods::default(), repeat: false });
        r.pump_until("the folder", |a| a.rows.len() == 3);
        press_on_row(&mut r, 1);
        r.app.input(Input::Move { x: -5.0, y: 200.0 });
        let l = r.app.layout();
        let y = (l.list.y + l.row_h / 2) as f64;
        r.app.input(Input::DragEnter { x: 540.0, y, files: true, can_copy: true, can_move: true, marker: None });
        r.app.input(Input::DragMove { x: 540.0, y });
        r.app.input(Input::OutDragDropped);
        assert!(r.app.file_drag.is_some(), "the drop is on its way");
        r.app.input(Input::DragDrop);
        assert!(r.app.file_drag.is_none());
        r.pump_until("the job", |a| a.jobs.is_empty());
        assert!(r.root.join("work/d/a").exists(), "it landed");
    }

    #[test]
    fn keys_left_over_when_focus_goes_dont_count() {
        let mut r = drag_rig("in-keys");
        r.app.input(Input::Mods(Mods { ctrl: true, ..Mods::default() }));
        r.app.input(Input::Focus(false));
        assert!(!r.app.mods_now.ctrl);
    }

    #[test]
    fn dropping_files_where_they_already_are_does_nothing() {
        let mut r = drag_rig("in-same");
        r.app.on_msg(Msg::Dropped(dropped(&r, &["work/a"], false, true)));
        assert!(r.app.jobs.is_empty() && r.app.dialog.is_none());
        assert!(r.root.join("work/a").exists());
        assert_eq!(answered(&r.app), (false, true), "saying it worked could make a source delete them");
    }

    #[test]
    fn a_drop_that_brings_no_files_says_so() {
        let mut r = drag_rig("in-none");
        r.app.on_msg(Msg::Dropped(dropped(&r, &[], true, true)));
        assert!(r.app.jobs.is_empty());
        assert!(r.app.notices.iter().any(|n| n.contains("Couldn't get any files")));
        assert_eq!(answered(&r.app), (false, true), "the other app still hears back");
    }

    #[test]
    fn trash_needs_the_other_app_to_allow_a_move() {
        let mut r = drag_rig("in-trash");
        outside_files(&r);
        let mut d = dropped(&r, &["outside/x"], true, false);
        d.trash = true;
        r.app.on_msg(Msg::Dropped(d));
        assert!(r.root.join("outside/x").exists());
        assert!(r.app.jobs.is_empty());
        let mut d = dropped(&r, &["outside/x"], true, true);
        d.trash = true;
        r.app.on_msg(Msg::Dropped(d));
        r.pump_until("the Trash", |a| a.jobs.is_empty());
        assert!(!r.root.join("outside/x").exists() && r.root.join("data/Trash/info/x.trashinfo").exists());
    }

    #[test]
    fn dragging_out_offers_what_the_keys_ask_for() {
        for (tag, mods, want) in [("out-plain", Mods::default(), false), ("out-shift", Mods { shift: true, ..Mods::default() }, true), ("out-ctrl", Mods { ctrl: true, ..Mods::default() }, false), ("out-both", Mods { ctrl: true, shift: true, ..Mods::default() }, false)] {
            let mut r = dragging(tag);
            r.app.input(Input::Mods(mods));
            r.app.cmds.clear();
            r.app.input(Input::Leave);
            assert!(r.app.cmds.iter().any(|c| matches!(c, Cmd::StartDrag { move_only, .. } if *move_only == want)), "{tag}");
        }
    }

    #[test]
    fn items_in_the_trash_only_leave_as_copies_even_with_shift() {
        let mut r = drag_rig("out-trash");
        fs::create_dir_all(r.root.join("data/Trash/files")).unwrap();
        fs::write(r.root.join("data/Trash/files/t"), b"t").unwrap();
        r.app.navigate(r.root.join("data/Trash/files"), None);
        r.pump_until("the Trash", |a| a.rows.len() == 1 && !a.loading);
        press_on_row(&mut r, 0);
        r.app.input(Input::Move { x: 540.0, y: 500.0 });
        r.app.input(Input::Mods(Mods { shift: true, ..Mods::default() }));
        r.app.cmds.clear();
        r.app.input(Input::Leave);
        assert!(r.app.cmds.iter().any(|c| matches!(c, Cmd::StartDrag { move_only: false, marker: None, .. })));
    }

    #[test]
    fn a_refused_drag_doesnt_stay_stuck() {
        // A release can't arrive during a compositor's drag.
        let mut r = dragging("out-refused-release");
        r.app.input(Input::Leave);
        r.app.cmds.clear();
        r.app.input(Input::Button { button: BTN_LEFT, pressed: false });
        assert!(r.app.file_drag.is_none());
        assert!(r.app.cmds.iter().any(|c| matches!(c, Cmd::CancelDrag)));
        assert!(r.app.jobs.is_empty());
        // Nor can pointer events, apart from ones already on their way.
        let mut r = dragging("out-refused-move");
        r.app.input(Input::Leave);
        r.app.input(Input::Move { x: 300.0, y: 300.0 });
        assert!(r.app.file_drag.is_some(), "too soon to tell");
        std::thread::sleep(WAYLAND_DRAG_GRACE + Duration::from_millis(50));
        r.app.input(Input::Move { x: 310.0, y: 300.0 });
        assert!(r.app.file_drag.is_none());
    }

    #[test]
    fn a_drop_elsewhere_ends_the_drag_but_keeps_the_source_for_the_reader() {
        let mut r = dragging("out-dropped");
        r.app.input(Input::Leave);
        r.app.cmds.clear();
        r.app.input(Input::OutDragDropped);
        assert!(r.app.file_drag.is_none());
        assert!(!r.app.cmds.iter().any(|c| matches!(c, Cmd::CancelDrag)), "the other app is still reading");
    }

    #[test]
    fn a_stray_drop_is_let_go() {
        let mut r = drag_rig("in-stray");
        r.app.input(Input::DragDrop);
        assert_eq!(answered(&r.app), (false, true));
    }

    #[test]
    fn something_that_isnt_files_is_refused() {
        let mut r = drag_rig("in-text");
        r.app.cmds.clear();
        r.app.input(Input::DragEnter { x: 540.0, y: 500.0, files: false, can_copy: true, can_move: true, marker: None });
        assert!(!r.app.cmds.iter().any(|c| matches!(c, Cmd::AcceptDrop(true))));
        r.app.input(Input::DragDrop);
        assert!(!r.app.cmds.iter().any(|c| matches!(c, Cmd::ReadDrop { .. })));
        assert_eq!(answered(&r.app), (false, true));
    }

    #[test]
    fn incoming_drops_aim_at_folders_then_the_folder_showing() {
        let mut r = drag_rig("in-aim");
        fs::create_dir(r.root.join("work/d")).unwrap();
        r.app.input(Input::Key { keysym: k::KEY_F5, utf8: String::new(), mods: Mods::default(), repeat: false });
        r.pump_until("the folder", |a| a.rows.len() == 3);
        let l = r.app.layout();
        let row = |i: i32| (l.list.y + i * l.row_h + l.row_h / 2) as f64;
        let dest = |a: &App| a.incoming.as_ref().and_then(|i| i.target.as_ref()).map(|t| t.dest.clone());
        r.app.input(Input::DragEnter { x: 540.0, y: row(0), files: true, can_copy: true, can_move: true, marker: None });
        r.app.input(Input::DragMove { x: 540.0, y: row(0) });
        assert_eq!(dest(&r.app), Some(r.root.join("work/d")));
        r.app.input(Input::DragMove { x: 540.0, y: row(1) });
        assert_eq!(dest(&r.app), Some(r.root.join("work")), "a file means the folder it's in");
        r.app.input(Input::DragMove { x: 540.0, y: row(9) });
        assert_eq!(dest(&r.app), Some(r.root.join("work")));
        r.app.input(Input::DragLeave);
        assert!(r.app.incoming.is_none());
    }

    #[test]
    fn a_modal_refuses_an_incoming_drop() {
        let mut r = drag_rig("in-modal");
        r.app.dialog = Some(Dialog::ConfirmQuit { at: 0 });
        r.app.input(Input::DragEnter { x: 540.0, y: 500.0, files: true, can_copy: true, can_move: true, marker: None });
        assert!(r.app.incoming.as_ref().is_some_and(|i| i.target.is_none()));
    }

    #[test]
    fn a_dialog_opening_after_a_press_keeps_the_selection() {
        let mut r = drag_rig("drag-dialog-press");
        r.app.input(Input::Key { keysym: k::KEY_a, utf8: "a".into(), mods: Mods { ctrl: true, ..Mods::default() }, repeat: false });
        assert_eq!(r.app.sel_count, 2);
        press_on_row(&mut r, 0);
        assert!(matches!(r.app.file_drag, Some(FileDrag::Pressed { .. })));
        r.app.dialog = Some(Dialog::ConfirmQuit { at: 0 });
        r.app.input(Input::Resized);
        assert!(r.app.file_drag.is_none());
        r.app.input(Input::Button { button: BTN_LEFT, pressed: false });
        assert_eq!(r.app.sel_count, 2, "nothing changes behind the dialog");
    }

    #[test]
    fn any_input_ends_a_drag_once_a_dialog_is_up() {
        let mut r = dragging("drag-dialog-input");
        r.app.dialog = Some(Dialog::ConfirmQuit { at: 0 });
        r.app.input(Input::Resized);
        assert!(r.app.file_drag.is_none());
    }

    #[test]
    fn a_release_under_a_dialog_starts_nothing() {
        let mut r = drag_rig("drag-dialog-release");
        fs::create_dir(r.root.join("work/d")).unwrap();
        r.app.input(Input::Key { keysym: k::KEY_F5, utf8: String::new(), mods: Mods::default(), repeat: false });
        r.pump_until("the folder", |a| a.rows.len() == 3);
        // Folders sort first, so the folder is row 0 and the files follow.
        press_on_row(&mut r, 1);
        let l = r.app.layout();
        r.app.input(Input::Move { x: 540.0, y: (l.list.y + l.row_h / 2) as f64 });
        assert!(matches!(r.app.file_drag, Some(FileDrag::Active(_))));
        r.app.dialog = Some(Dialog::ConfirmQuit { at: 0 });
        r.app.input(Input::Button { button: BTN_LEFT, pressed: false });
        assert!(r.app.jobs.is_empty(), "nothing moves under a dialog");
        assert!(r.root.join("work/a").exists());
        assert!(r.app.file_drag.is_none());
    }

    #[test]
    fn the_move_that_ends_a_drag_reaches_the_menu() {
        let mut r = dragging("drag-menu-move");
        r.app.open_menu(800, 40);
        let m = r.app.menu.as_ref().unwrap();
        let rect = r.app.menu_rect(m);
        let y = rect.y + r.app.px(4.0) + r.app.px(14.0);
        // Straight to the handler, so this move is what notices the menu.
        r.app.pointer_moved((rect.x + r.app.px(10.0)) as f64, y as f64);
        assert!(r.app.file_drag.is_none());
        assert_eq!(r.app.menu.as_ref().unwrap().at, 0);
    }

    #[test]
    fn the_same_cursor_isnt_asked_for_twice() {
        let mut r = dragging("drag-repeat");
        r.app.cmds.clear();
        r.app.input(Input::Move { x: 560.0, y: 520.0 });
        r.app.input(Input::Move { x: 580.0, y: 540.0 });
        assert!(cursors(&r.app).is_empty(), "{:?}", cursors(&r.app));
    }

    fn ghost_of(a: &App) -> (Vec<u8>, usize, Rect, Rect) {
        let Some(FileDrag::Active(d)) = &a.file_drag else { panic!("no drag") };
        (d.ghost.name.clone(), d.ghost.count, d.ghost_at, d.label)
    }

    #[test]
    fn a_drag_carries_a_row_and_only_labels_what_a_drop_would_do() {
        let mut r = drag_rig("ghost-row");
        fs::create_dir(r.root.join("work/d")).unwrap();
        r.app.input(Input::Key { keysym: k::KEY_F5, utf8: String::new(), mods: Mods::default(), repeat: false });
        r.pump_until("the folder", |a| a.rows.len() == 3);
        press_on_row(&mut r, 1);
        r.app.input(Input::Move { x: 540.0, y: 500.0 });
        let (name, count, at, label) = ghost_of(&r.app);
        assert_eq!((name.as_slice(), count), (&b"a"[..], 1));
        assert!(!at.is_empty() && label.is_empty(), "no target, so no label");
        let l = r.app.layout();
        r.app.input(Input::Move { x: 540.0, y: (l.list.y + l.row_h / 2) as f64 });
        let (_, _, at, label) = ghost_of(&r.app);
        assert!(!label.is_empty() && label.y >= at.bottom(), "the label sits under the row");
    }

    #[test]
    fn the_row_carried_is_the_one_pressed_with_a_count() {
        let mut r = drag_rig("ghost-count");
        r.app.input(Input::Key { keysym: k::KEY_a, utf8: "a".into(), mods: Mods { ctrl: true, ..Mods::default() }, repeat: false });
        press_on_row(&mut r, 1);
        r.app.input(Input::Move { x: 540.0, y: 500.0 });
        let (name, count, ..) = ghost_of(&r.app);
        assert_eq!((name.as_slice(), count), (&b"b"[..], 2));
    }

    #[test]
    fn a_drag_near_the_corner_stays_in_the_window() {
        let mut r = dragging("ghost-corner");
        let (w, h) = (r.app.w as f64, r.app.h as f64);
        r.app.input(Input::Move { x: w - 2.0, y: h - 2.0 });
        let (_, _, at, _) = ghost_of(&r.app);
        assert!(at.right() <= r.app.w && at.bottom() <= r.app.h, "{at:?}");
    }

    #[test]
    fn leaving_hands_the_compositor_a_picture_of_the_row() {
        let mut r = dragging("ghost-icon");
        r.app.cmds.clear();
        r.app.input(Input::Leave);
        let Some(Cmd::StartDrag { icon: Some(icon), .. }) = r.app.cmds.iter().find(|c| matches!(c, Cmd::StartDrag { .. })) else { panic!("no picture") };
        assert_eq!(icon.pixels.len(), (icon.w * icon.h) as usize);
        assert!(icon.pixels.iter().any(|&p| p != icon.pixels[0]), "something's drawn");
        assert!(ghost_of(&r.app).2.is_empty(), "the compositor draws it now");
    }

    #[test]
    fn a_drag_ending_where_the_hover_is_unchanged_still_resets_the_cursor() {
        let mut r = dragging("drag-same-hover");
        // The empty sidebar: no hit, the same as the hover a drag starts with.
        r.app.input(Input::Move { x: 50.0, y: 500.0 });
        assert_eq!(r.app.hover, Hit::None);
        r.app.cmds.clear();
        r.app.input(Input::Key { keysym: k::KEY_Escape, utf8: String::new(), mods: Mods::default(), repeat: false });
        assert!(r.app.file_drag.is_none());
        assert_eq!(cursors(&r.app), [Cursor::Default]);
    }

    #[test]
    fn hover_is_right_after_a_drag_ends_over_a_row() {
        let mut r = drag_rig("drag-hover");
        press_on_row(&mut r, 0);
        let l = r.app.layout();
        let y = l.list.y + l.row_h + l.row_h / 2;
        r.app.input(Input::Move { x: 540.0, y: y as f64 });
        assert!(matches!(r.app.file_drag, Some(FileDrag::Active(_))));
        r.app.input(Input::Key { keysym: k::KEY_Escape, utf8: String::new(), mods: Mods::default(), repeat: false });
        assert_eq!(r.app.hover, Hit::Row(1));
    }

    #[test]
    fn a_new_folder_cancels_a_drag() {
        let mut r = drag_rig("drag-newdir");
        press_on_row(&mut r, 0);
        r.app.input(Input::Move { x: 540.0, y: 200.0 });
        assert!(matches!(r.app.file_drag, Some(FileDrag::Active(_))));
        let elsewhere = r.root.clone();
        r.app.navigate(elsewhere, None);
        r.app.on_msg(Msg::Notice("anything".into()));
        assert!(r.app.file_drag.is_none());
    }

    #[test]
    fn losing_focus_cancels_a_drag() {
        let mut r = drag_rig("drag-focus");
        press_on_row(&mut r, 0);
        r.app.input(Input::Move { x: 540.0, y: 200.0 });
        assert!(matches!(r.app.file_drag, Some(FileDrag::Active(_))));
        r.app.input(Input::Focus(false));
        assert!(r.app.file_drag.is_none());
        r.app.input(Input::Button { button: BTN_LEFT, pressed: false });
        assert!(r.app.jobs.is_empty());
    }

    #[test]
    fn a_release_only_narrows_the_selection_if_the_row_is_still_there() {
        let mut r = drag_rig("drag-narrow");
        fs::write(r.root.join("work/c"), b"3").unwrap();
        r.app.input(Input::Key { keysym: k::KEY_F5, utf8: String::new(), mods: Mods::default(), repeat: false });
        r.pump_until("the third file", |a| a.rows.len() == 3);
        r.app.input(Input::Key { keysym: k::KEY_a, utf8: "a".into(), mods: Mods { ctrl: true, ..Mods::default() }, repeat: false });
        assert_eq!(r.app.sel_count, 3);
        press_on_row(&mut r, 1);
        assert_eq!(r.app.sel_count, 3, "the press keeps the selection");
        // The pressed row goes, so another file is at that position now.
        fs::remove_file(r.root.join("work/b")).unwrap();
        r.app.input(Input::Key { keysym: k::KEY_F5, utf8: String::new(), mods: Mods::default(), repeat: false });
        r.pump_until("the reload", |a| a.rows.len() == 2);
        assert_eq!(r.app.sel_count, 2);
        r.app.input(Input::Button { button: BTN_LEFT, pressed: false });
        assert_eq!(r.app.sel_count, 2, "narrowing would have selected a file that wasn't pressed");
        assert!(r.app.file_drag.is_none());
    }

    #[test]
    fn the_trash_folder_is_read_only_apart_from_copying_out_and_deleting() {
        let mut r = Rig::new("trash-ro");
        let files = r.root.join("data/Trash/files");
        fs::create_dir_all(&files).unwrap();
        fs::write(files.join("t"), "x").unwrap();
        r.open(files.clone());
        r.select("t");
        for a in [Action::Rename, Action::Cut, Action::Paste, Action::NewFolder] {
            r.app.notices.clear();
            r.app.action(a);
            assert!(!r.app.notices.is_empty(), "{a:?} should be refused");
            assert_eq!(r.app.edit, Edit::None);
        }
        assert!(!r.app.cmds.iter().any(|c| matches!(c, Cmd::ReadClipboard(_) | Cmd::SetClipboard(_))));
        // Delete asks, because there's no further Trash to go to.
        r.key(k::KEY_Delete, "");
        assert!(matches!(r.app.dialog, Some(Dialog::ConfirmDelete { .. })));
    }

    #[test]
    fn ctrl_shift_c_copies_the_path_and_ctrl_shift_z_does_nothing() {
        let mut r = Rig::new("shift-keys");
        fs::write(r.work().join("a"), "").unwrap();
        r.open(r.work());
        r.select("a");
        r.app.cmds.clear();
        let shift = Mods { ctrl: true, shift: true, ..Mods::default() };
        r.app.key(k::KEY_C, "", shift);
        let Some(Cmd::SetClipboard(offers)) = r.app.cmds.pop() else { panic!("nothing copied") };
        assert!(offers.iter().all(|(m, _)| *m != clipboard::GNOME), "text only, not a file copy");
        r.app.undo.push(Undo::Rename { from: "/a".into(), to: "/b".into() });
        r.app.key(k::KEY_Z, "", shift);
        r.app.key(k::KEY_z, "", shift);
        assert_eq!(r.app.undo.len(), 1, "Ctrl+Shift+Z must not undo");
    }

    #[test]
    fn losing_the_clipboard_undims_cut_rows() {
        let mut r = Rig::new("clip-lost");
        fs::write(r.work().join("a"), "").unwrap();
        r.open(r.work());
        r.select("a");
        r.ctrl(k::KEY_x);
        assert_eq!(r.app.cut_paths.len(), 1);
        r.app.input(Input::ClipboardLost);
        assert!(r.app.cut_paths.is_empty());
    }

    /// A folder of plain files with the cursor on the first, ready to preview.
    fn preview_rig(tag: &str) -> Rig {
        let mut r = Rig::new(tag);
        for name in ["a", "b", "c"] {
            fs::write(r.work().join(name), "x").unwrap();
        }
        r.open(r.work());
        r
    }

    fn previewing(r: &Rig) -> Option<String> {
        r.app.preview.as_ref().map(|p| p.path.file_name().unwrap().to_string_lossy().into_owned())
    }

    fn with_mods(r: &mut Rig, sym: u32, utf8: &str, mods: Mods) {
        r.app.input(Input::Key { keysym: sym, utf8: utf8.into(), mods, repeat: false });
    }

    #[test]
    fn space_previews_and_closes_and_leaves_the_selection_alone() {
        let mut r = preview_rig("space");
        r.key(k::KEY_space, " ");
        assert_eq!(previewing(&r).as_deref(), Some("a"));
        assert_eq!(r.selected_names(), ["a"], "showing the first row selects it, as arrows do");
        r.key(k::KEY_space, " ");
        assert_eq!(previewing(&r), None);
        assert_eq!(r.selected_names(), ["a"]);
        r.key(k::KEY_space, " ");
        r.key(k::KEY_Escape, "");
        assert_eq!(previewing(&r), None);
        assert_eq!(r.selected_names(), ["a"], "Escape closes the popup first, and nothing else");
    }

    #[test]
    fn ctrl_space_toggles_selection_without_a_popup() {
        let mut r = preview_rig("ctrl-space");
        let ctrl = Mods { ctrl: true, ..Mods::default() };
        with_mods(&mut r, k::KEY_space, " ", ctrl);
        assert_eq!(previewing(&r), None);
        assert_eq!(r.selected_names(), ["a"]);
        with_mods(&mut r, k::KEY_Down, "", ctrl);
        with_mods(&mut r, k::KEY_space, " ", ctrl);
        assert_eq!(r.selected_names(), ["a", "b"]);
        with_mods(&mut r, k::KEY_space, " ", ctrl);
        assert_eq!(r.selected_names(), ["a"]);
    }

    #[test]
    fn holding_space_doesnt_flicker() {
        let mut r = preview_rig("repeat");
        let space = |repeat| Input::Key { keysym: k::KEY_space, utf8: " ".into(), mods: Mods::default(), repeat };
        r.app.input(space(false));
        r.app.input(space(true));
        assert_eq!(previewing(&r).as_deref(), Some("a"), "a held key doesn't close it");
        r.app.input(space(false));
        assert_eq!(previewing(&r), None);
        r.app.input(space(true));
        assert_eq!(previewing(&r), None, "or open it again");
    }

    #[test]
    fn arrows_walk_the_files_and_the_ends_stay_put() {
        let mut r = preview_rig("arrows");
        r.key(k::KEY_space, " ");
        r.key(k::KEY_Up, "");
        assert_eq!(previewing(&r).as_deref(), Some("a"));
        r.key(k::KEY_Down, "");
        assert_eq!(previewing(&r).as_deref(), Some("b"));
        r.key(k::KEY_Right, "");
        assert_eq!(previewing(&r).as_deref(), Some("c"));
        r.key(k::KEY_Down, "");
        assert_eq!(previewing(&r).as_deref(), Some("c"));
        r.key(k::KEY_Left, "");
        assert_eq!(previewing(&r).as_deref(), Some("b"));
        assert_eq!(r.selected_names(), ["b"], "the selection follows, so Enter opens what's showing");
    }

    #[test]
    fn walking_doesnt_lose_a_selection_of_several() {
        let mut r = preview_rig("several");
        r.app.select_all();
        r.key(k::KEY_space, " ");
        r.key(k::KEY_Down, "");
        assert_eq!(previewing(&r).as_deref(), Some("b"));
        assert_eq!(r.selected_names(), ["a", "b", "c"]);
    }

    #[test]
    fn other_keys_dont_reach_the_list_under_the_popup() {
        let mut r = preview_rig("keys");
        r.key(k::KEY_space, " ");
        r.key(k::KEY_c, "c");
        r.key(k::KEY_Delete, "");
        r.key(k::KEY_F2, "");
        assert_eq!(r.app.cursor, 0, "typing didn't jump");
        assert!(r.app.jobs.is_empty(), "Delete didn't trash");
        assert!(matches!(r.app.edit, Edit::None), "F2 didn't rename");
        assert!(r.work().join("a").exists());
    }

    #[test]
    fn nothing_to_preview_in_an_empty_folder() {
        let mut r = Rig::new("empty");
        r.open(r.work());
        r.key(k::KEY_space, " ");
        assert_eq!(previewing(&r), None);
    }

    #[test]
    fn a_file_removed_underneath_moves_on_and_an_empty_folder_closes() {
        let mut r = preview_rig("removed");
        r.key(k::KEY_space, " ");
        fs::remove_file(r.work().join("a")).unwrap();
        r.app.reload();
        r.pump_until("the next file", |a| a.preview.as_ref().is_some_and(|p| p.path.ends_with("b")));
        for name in ["b", "c"] {
            fs::remove_file(r.work().join(name)).unwrap();
        }
        r.app.reload();
        r.pump_until("the popup to close", |a| a.preview.is_none());
    }

    #[test]
    fn a_file_overwritten_while_showing_is_read_again() {
        let mut r = preview_rig("overwritten");
        r.pump_until("a size for the first row", |a| a.listing.as_ref().is_some_and(|l| matches!(l.stat(a.rows[0]), Stat::Known { .. })));
        r.key(k::KEY_space, " ");
        let before = r.app.preview_gen;
        // Nothing changed: reloading leaves it alone, so a busy folder doesn't restart it.
        r.app.reload();
        r.pump_until("the reload", |a| !a.loading);
        r.pump_until("the new size", |a| a.listing.as_ref().is_some_and(|l| matches!(l.stat(a.rows[0]), Stat::Known { .. })));
        r.idle();
        assert_eq!(r.app.preview_gen, before, "the same file isn't read twice");
        fs::write(r.work().join("a"), "a good deal longer").unwrap();
        r.app.reload();
        r.pump_until("reading it again", |a| a.preview_gen > before);
        assert_eq!(previewing(&r).as_deref(), Some("a"));
    }

    #[test]
    fn holding_escape_after_closing_leaves_the_selection() {
        let mut r = preview_rig("held-escape");
        let escape = |repeat| Input::Key { keysym: k::KEY_Escape, utf8: String::new(), mods: Mods::default(), repeat };
        r.key(k::KEY_space, " ");
        r.app.input(escape(false));
        assert_eq!(previewing(&r), None);
        r.app.input(escape(true));
        assert_eq!(r.selected_names(), ["a"], "the repeats don't reach the list");
        r.app.input(escape(false));
        assert!(r.selected_names().is_empty(), "a new press does");
    }

    #[test]
    fn right_clicking_a_selected_row_previews_that_row() {
        let mut r = preview_rig("right-click");
        r.app.select_all();
        r.app.resized(1000, 700, 1.0);
        let l = r.app.layout();
        let (x, y) = (l.list.x + 50, l.list.y + l.row_h + l.row_h / 2);
        r.app.input(Input::Move { x: x as f64, y: y as f64 });
        r.app.input(Input::Button { button: BTN_RIGHT, pressed: true });
        assert!(r.app.menu.is_some());
        assert_eq!(r.selected_names(), ["a", "b", "c"], "the selection stays");
        r.app.close_menu();
        r.app.action(Action::Preview);
        assert_eq!(previewing(&r).as_deref(), Some("b"));
    }

    #[test]
    fn a_click_before_the_popup_appears_still_lands() {
        let mut r = preview_rig("early-click");
        r.app.resized(1000, 700, 1.0);
        r.key(k::KEY_space, " ");
        assert_eq!(r.app.preview_rect(), Rect::default(), "nothing drawn yet");
        let l = r.app.layout();
        r.app.input(Input::Move { x: (l.list.x + 50) as f64, y: (l.list.y + l.row_h + l.row_h / 2) as f64 });
        r.app.input(Input::Button { button: BTN_LEFT, pressed: true });
        assert_eq!(previewing(&r), None);
        assert_eq!(r.selected_names(), ["b"], "the click selected the row it was on");
    }

    #[test]
    fn a_report_from_before_a_key_press_doesnt_undo_it() {
        use crate::media::{PlayState, parts_for_test};
        use crate::preview::{Content, Event, Found};
        let mut r = preview_rig("stale-report");
        r.key(k::KEY_space, " ");
        let generation = r.app.preview_gen;
        let (remote, frames) = parts_for_test();
        r.app.on_msg(Msg::Preview(Event { generation, found: Found::VideoStarted { remote, frames } }));
        let playing = PlayState { position: 1.0, duration: 9.0, paused: false, volume: 100.0, muted: false };
        let report = |state| Msg::Preview(Event { generation, found: Found::VideoState(state) });
        let shown = |r: &Rig| match &r.app.preview.as_ref().unwrap().content {
            Content::Video(video) => video.state,
            _ => panic!("no video"),
        };
        r.app.on_msg(report(playing));
        r.key(k::KEY_p, "p");
        assert!(shown(&r).paused);
        // On its way before the press landed.
        r.app.on_msg(report(PlayState { position: 2.0, ..playing }));
        assert!(shown(&r).paused, "the press stands");
        assert_eq!(shown(&r).position, 2.0, "and the time still moves");
        r.key(k::KEY_p, "p");
        assert!(!shown(&r).paused, "the second press undoes the first, rather than repeating it");
        // Once things have settled, the player is right about itself.
        std::thread::sleep(Duration::from_millis(300));
        r.app.on_msg(report(PlayState { paused: true, ..playing }));
        assert!(shown(&r).paused);
    }

    #[test]
    fn the_context_menu_can_preview() {
        let mut r = preview_rig("menu");
        r.key(k::KEY_Down, "");
        r.app.action(Action::Preview);
        assert_eq!(previewing(&r).as_deref(), Some("a"));
    }

    #[test]
    fn answers_for_a_file_youve_moved_past_are_dropped() {
        let mut r = preview_rig("stale");
        r.key(k::KEY_space, " ");
        let old = r.app.preview_gen;
        r.key(k::KEY_Down, "");
        r.app.on_msg(Msg::Preview(preview::Event { generation: old, found: preview::Found::Message("old news".into()) }));
        assert!(matches!(r.app.preview.as_ref().unwrap().content, preview::Content::Waiting));
    }

    fn item(enabled: bool) -> MenuItem {
        MenuItem { label: "", keys: "", action: Action::Reload, enabled }
    }

    #[test]
    fn menu_keys_skip_disabled_items_and_wrap() {
        let items = vec![item(false), item(true), item(false), item(true)];
        assert_eq!(next_enabled(&items, usize::MAX, 1), 1);
        assert_eq!(next_enabled(&items, 1, 1), 3);
        assert_eq!(next_enabled(&items, 3, 1), 1);
        assert_eq!(next_enabled(&items, 3, 3), 1);
        assert_eq!(next_enabled(&items, 1, 3), 3);
        assert_eq!(next_enabled(&[item(false)], usize::MAX, 1), usize::MAX);
    }

    fn trash_names(r: &mut Rig, names: &[&str]) {
        for n in names {
            fs::write(r.work().join(n), n.as_bytes()).unwrap();
        }
        r.open(r.work());
        for n in names {
            r.select(n);
            r.key(k::KEY_Delete, "");
            r.idle();
        }
    }

    fn open_trash(r: &mut Rig) {
        let t = r.app.trash_dir.clone();
        r.open(t);
        assert!(r.app.listing.as_ref().unwrap().is_trash());
    }

    #[test]
    fn the_trash_shows_where_items_came_from_and_restores_them() {
        let mut r = Rig::new("trash-restore");
        trash_names(&mut r, &["a.txt", "b.txt"]);
        open_trash(&mut r);
        let l = r.app.listing.clone().unwrap();
        let a = (0..l.len() as u32).find(|&i| l.name(i) == b"a.txt").unwrap();
        assert_eq!(l.original(a), Some(fs::canonicalize(r.work()).unwrap().join("a.txt").as_path()));
        r.select("a.txt");
        r.app.action(Action::Restore);
        r.pump_until("restored and reloaded", |a| a.jobs.is_empty() && !a.loading && a.listing.as_ref().is_some_and(|l| l.len() == 1));
        assert_eq!(fs::read(r.work().join("a.txt")).unwrap(), b"a.txt");
        assert!(r.app.notices.is_empty(), "{:?}", r.app.notices);
    }

    #[test]
    fn restoring_onto_something_new_leaves_both_and_says_why() {
        let mut r = Rig::new("trash-restore-clash");
        trash_names(&mut r, &["a.txt"]);
        fs::write(r.work().join("a.txt"), b"new").unwrap();
        open_trash(&mut r);
        r.select("a.txt");
        r.app.action(Action::Restore);
        r.idle();
        assert_eq!(fs::read(r.work().join("a.txt")).unwrap(), b"new");
        assert_eq!(r.app.listing.as_ref().unwrap().len(), 1);
        assert!(r.app.notices.iter().any(|n| n.contains("stayed in the Trash")), "{:?}", r.app.notices);
    }

    #[test]
    fn emptying_the_trash_asks_first_then_takes_hidden_items_too() {
        let mut r = Rig::new("trash-empty");
        trash_names(&mut r, &["a.txt"]);
        r.app.show_hidden = true;
        trash_names(&mut r, &[".h"]);
        r.app.show_hidden = false;
        open_trash(&mut r);
        assert_eq!(r.app.rows.len(), 1, "the hidden item isn't showing");
        r.app.action(Action::EmptyTrash);
        assert!(matches!(&r.app.dialog, Some(Dialog::ConfirmDelete { empty: true, paths, .. }) if paths.len() == 2));
        r.key(k::KEY_Escape, "");
        assert!(r.app.dialog.is_none());
        assert_eq!(fs::read_dir(r.root.join("data/Trash/files")).unwrap().count(), 2, "Cancel deletes nothing");
        r.app.action(Action::EmptyTrash);
        r.key(k::KEY_e, "e");
        r.pump_until("emptied", |a| a.jobs.is_empty() && !a.loading && a.listing.as_ref().is_some_and(|l| l.is_empty()));
        for sub in ["files", "info"] {
            assert_eq!(fs::read_dir(r.root.join("data/Trash").join(sub)).unwrap().count(), 0, "{sub}");
        }
    }

    #[test]
    fn the_trash_menu_offers_restore_and_empty_and_nothing_that_would_change_items() {
        let mut r = Rig::new("trash-menu");
        trash_names(&mut r, &["a.txt"]);
        open_trash(&mut r);
        r.select("a.txt");
        let labels: Vec<&str> = r.app.context_items().iter().map(|m| m.label).collect();
        for want in ["Restore", "Empty Trash", "Delete for good", "Sort by date deleted"] {
            assert!(labels.contains(&want), "{want} missing from {labels:?}");
        }
        for unwanted in ["Cut", "Paste", "Rename", "Move to Trash", "New folder"] {
            assert!(!labels.contains(&unwanted), "{unwanted} in {labels:?}");
        }
    }

    #[test]
    fn up_stays_out_of_the_trashs_own_folders() {
        let mut r = Rig::new("trash-up");
        fs::create_dir_all(r.root.join("work/d/inner")).unwrap();
        r.open(r.work());
        r.select("d");
        r.key(k::KEY_Delete, "");
        r.idle();
        open_trash(&mut r);
        assert!(r.app.up_target().is_none(), "nothing above the Trash view");
        let d = r.app.trash_dir.join("d");
        r.open(d.join("inner"));
        assert_eq!(r.app.up_target().map(|s| s.dir), Some(d.clone()));
        r.open(d);
        assert_eq!(r.app.up_target().map(|s| s.dir), Some(r.app.trash_dir.clone()));
        let other = r.root.join("disk/.Trash-1000/files/x");
        fs::create_dir_all(&other).unwrap();
        r.open(other);
        assert_eq!(r.app.up_target().map(|s| s.dir), Some(r.app.trash_dir.clone()), "another disk's Trash goes up to the Trash view too");
    }

    #[test]
    fn the_empty_trash_button_shows_only_in_the_trash_view() {
        let mut r = Rig::new("trash-button");
        trash_names(&mut r, &["a.txt"]);
        assert!(r.app.empty_trash_rect().is_none());
        open_trash(&mut r);
        let b = r.app.empty_trash_rect().unwrap();
        assert_eq!(r.app.hit(b.x + b.w / 2, b.y + b.h / 2), Hit::EmptyTrash);
        assert!(r.app.crumbs().iter().all(|(x, w, _)| x + w <= b.x), "the path stops before the button");
        r.app.input(Input::Move { x: (b.x + 4) as f64, y: (b.y + 4) as f64 });
        r.app.input(Input::Button { button: BTN_LEFT, pressed: true });
        r.app.input(Input::Button { button: BTN_LEFT, pressed: false });
        assert!(matches!(r.app.dialog, Some(Dialog::ConfirmDelete { empty: true, .. })));
    }

    #[test]
    fn a_reload_keeps_the_selection_on_the_same_item_when_two_share_a_name() {
        let mut r = Rig::new("trash-same-name");
        trash_names(&mut r, &["a.txt"]);
        trash_names(&mut r, &["a.txt"]);
        open_trash(&mut r);
        let l = r.app.listing.clone().unwrap();
        // The lowest id is the one a lookup by name alone would lose.
        let id = (0..l.len() as u32).find(|&i| l.name(i) == b"a.txt").unwrap();
        let want = l.path(id);
        let row = r.app.rows.iter().position(|&i| i == id).unwrap();
        r.app.move_cursor(row, Mods::default());
        r.app.reload();
        r.pump_until("reloaded", |a| !a.loading && !a.listing.as_ref().is_some_and(|n| Arc::ptr_eq(n, &l)));
        let l = r.app.listing.clone().unwrap();
        let picked: Vec<PathBuf> = r.app.selected_ids().into_iter().map(|i| l.path(i)).collect();
        assert_eq!(picked, std::slice::from_ref(&want));
        assert_eq!(l.path(r.app.rows[r.app.cursor]), want);
    }

    #[test]
    fn the_path_field_stops_before_the_empty_trash_button() {
        let mut r = Rig::new("trash-path-box");
        trash_names(&mut r, &["a.txt"]);
        open_trash(&mut r);
        let b = r.app.empty_trash_rect().unwrap();
        assert!(r.app.path_box().right() <= b.x);
    }

    #[test]
    fn an_empty_trash_has_no_button_to_press() {
        let mut r = Rig::new("trash-empty-button");
        open_trash(&mut r);
        let b = r.app.empty_trash_rect().unwrap();
        assert_eq!(r.app.hit(b.x + b.w / 2, b.y + b.h / 2), Hit::None);
    }

    #[test]
    fn emptying_waits_until_every_disk_has_been_read() {
        let mut r = Rig::new("trash-empty-loading");
        trash_names(&mut r, &["a.txt"]);
        open_trash(&mut r);
        // As if another disk's Trash were still being read.
        r.app.loading = true;
        let b = r.app.empty_trash_rect().unwrap();
        assert_eq!(r.app.hit(b.x + b.w / 2, b.y + b.h / 2), Hit::None);
        assert!(!r.app.context_items().iter().any(|m| m.label == "Empty Trash" && m.enabled));
        r.app.action(Action::EmptyTrash);
        assert!(r.app.dialog.is_none());
    }

    #[test]
    fn the_path_inside_another_disks_trash_starts_at_trash() {
        let mut r = Rig::new("trash-crumbs");
        let inner = r.root.join("disk/.Trash-1000/files/d/e");
        fs::create_dir_all(&inner).unwrap();
        r.open(inner.clone());
        let crumbs: Vec<PathBuf> = r.app.crumbs().into_iter().map(|(_, _, p)| p).collect();
        assert_eq!(crumbs, [r.app.trash_dir.clone(), r.root.join("disk/.Trash-1000/files/d"), inner]);
    }

    #[test]
    fn leaving_a_folder_in_the_trash_puts_the_cursor_back_on_it() {
        let mut r = Rig::new("trash-back-to-row");
        fs::create_dir_all(r.root.join("work/d/inner")).unwrap();
        trash_names(&mut r, &["z.txt"]);
        r.open(r.work());
        r.select("d");
        r.key(k::KEY_Delete, "");
        r.idle();
        let d = r.app.trash_dir.join("d");
        r.open(d.clone());
        r.app.go_up();
        r.pump_until("back in the Trash", |a| !a.loading && a.is_trash_view());
        let l = r.app.listing.clone().unwrap();
        assert!(r.app.cursor_shown);
        assert_eq!(l.path(r.app.rows[r.app.cursor]), d);
    }

    #[test]
    fn items_asked_for_by_name_arrive_selected_hidden_ones_too() {
        let mut r = Rig::new("select-on-arrival");
        for n in ["a", "b", ".h"] {
            fs::write(r.work().join(n), b"").unwrap();
        }
        r.app.go(Stop { dir: r.work(), cursor_name: None });
        r.app.select_on_arrival(vec![b"b".to_vec(), b".h".to_vec()]);
        r.pump_until("loaded", |a| !a.loading);
        let mut got = r.selected_names();
        got.sort();
        assert_eq!(got, [".h", "b"]);
        assert!(r.app.show_hidden);
        let l = r.app.listing.clone().unwrap();
        assert_eq!(l.name(r.app.rows[r.app.cursor]), b"b", "the cursor is on the first");
    }
}
