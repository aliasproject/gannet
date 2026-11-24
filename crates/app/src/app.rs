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
use gannet_core::folder_sorts::FolderSorts;
use gannet_core::mounts::Mount;
use gannet_core::trash;
use gannet_core::ops::{self, Answer, Choice, Conflict, Engine, Job, Request, Undo};
use gannet_core::watch::Watch;
use gannet_core::disks::{DiskError, Volume};
use gannet_core::bookmarks::{self, Spot};
use gannet_core::places::{Place, PlaceKind};
use crate::drives::Doing;
use gannet_core::theme::{Colors, Source};
use rustix::fd::OwnedFd;
use xkbcommon::xkb::keysyms as k;

use crate::canvas::Rect;
use crate::field::Field;
use crate::icons::Icons;
use crate::preview::{self, Preview};
use crate::rename_many::{Focus, RenameMany};
use crate::text::Text;
use crate::wl::{Cursor, Input, Mods};

pub const BTN_LEFT: u32 = 0x110;
pub const BTN_RIGHT: u32 = 0x111;
pub const BTN_SIDE: u32 = 0x113;
pub const BTN_EXTRA: u32 = 0x114;

/// The height of a heading in the sidebar, like "Devices", and the space above each after the
/// first, in unscaled pixels.
pub const SIDEBAR_HEADING: f64 = 24.0;
pub const SIDEBAR_GAP: f64 = 10.0;
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
    /// What fontconfig chose for one of the preview's document fonts.
    DocFont(crate::docview::Role, PathBuf, u32),
    /// The drives udisks2 knows about now.
    Volumes(Vec<Volume>),
    /// udisks2 has stopped, so what it said about drives no longer holds. Unlike a drive going,
    /// that doesn't mean anything's been unmounted.
    NoUdisks,
    /// A mount, unlock, unmount or eject has finished: where it's mounted, for the first two.
    DiskDone { volume: Volume, doing: Doing, result: Result<Option<PathBuf>, DiskError> },
    DesktopDark(bool),
    /// Whether the desktop portal can set the background now.
    CanSetBackground(bool),
    Notice(String),
    Op(ops::Event),
    /// `data` is `None` if it was too big to trust.
    Clipboard(Option<Vec<u8>>, String, Paste),
    Preview(preview::Event),
    /// The preview window stepped to another file.
    PreviewStepped {
        generation: u64,
        path: PathBuf,
    },
    /// The preview window has gone, and whether it went wrong.
    PreviewEnded {
        generation: u64,
        crashed: bool,
    },
    /// In the preview window: the list's cursor is on another file, or its order has changed.
    PreviewShow(PathBuf, SortSpec),
    /// In the preview window: the list that opened it has gone.
    ListGone,
    /// A file list dropped from a Wayland drag, read and looked at off the main thread.
    Dropped(Dropped),
    DragSource { drag: u64, facts: Option<(u64, bool)> },
    DragDevice { drag: u64, path: PathBuf, device: Option<u64> },
    /// A screen reader asked for something.
    A11y(crate::a11y::Act),
    /// The folders among what was dropped to pin, and how many items weren't folders.
    PinDropped { folders: Vec<PathBuf>, at: Spot, not_folders: usize },
    /// A row's thumbnail, or that it hasn't one.
    Thumbnail(crate::thumbs::Done),
}

#[derive(Clone)]
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
    /// No files, but a picture, as a browser offers, saved as a new file: where, or why not.
    pub saved: Option<Result<PathBuf, String>>,
    /// No files and no picture, but a web address.
    pub link: bool,
    /// Where to pin what was dropped, rather than moving it.
    pub pin: Option<Spot>,
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
    ReadDrop { dest: PathBuf, trash: bool, decided: Option<DropAction>, pin: Option<Spot> },
    FinishDrop,
    AbandonDrop,
    /// Starts the preview window on a file, sorted and filtered as this list is.
    OpenPreview {
        generation: u64,
        path: PathBuf,
        hidden: bool,
        sort: SortSpec,
    },
    ClosePreview,
    /// Tells the preview window to show another file, in the list's order.
    PreviewShow(PathBuf, SortSpec),
    /// In the preview window: tells the list which file is showing now.
    PreviewStepped(PathBuf),
    /// Asks which app to open these files with, in a window over this one.
    OpenWith(Vec<PathBuf>),
    Quit,
}

/// A running operation shown in the panel once it's lasted long enough to matter.
pub struct JobView {
    pub job: Job,
    pub started: Instant,
    kind: JobKind,
    selection_epoch: u64,
    /// `App::change_epoch` when it started, so an undo that finishes after something new has
    /// begun doesn't offer a redo.
    change_epoch: u64,
}

/// What a finished job means for undo and redo.
#[derive(Clone, Copy, PartialEq)]
enum JobKind {
    /// Something new: what it did goes on the undo list.
    Change,
    /// Deleting for good, or a copy or link that left no undo (a copy that only merged, say): it can't be
    /// undone, and undoing something older right after would be a surprise. What a copy can
    /// undo still goes on the undo list.
    EndsUndo,
    /// What it reversed goes on the redo list.
    Undo,
    /// What it put back goes on the undo list again.
    Redo,
}

/// A question that has to be answered before anything else happens.
pub enum Dialog {
    Conflict { conflict: Conflict, apply_to_all: bool, at: usize },
    /// `empty` is for emptying the Trash, which words the question differently.
    ConfirmDelete { paths: Vec<PathBuf>, at: usize, empty: bool },
    ConfirmQuit { at: usize },
    /// Saving from a chooser onto files that are there. `existing` are the ones that are.
    ConfirmReplace { paths: Vec<PathBuf>, existing: Vec<PathBuf>, at: usize },
    /// The passphrase for an encrypted drive. `error` says why the last one didn't work, and
    /// `waiting` is set while udisks2 tries it.
    Unlock { volume: Volume, field: Field, at: usize, error: Option<String>, waiting: bool },
    /// Renaming the items selected, each old name beside its new one.
    RenameMany(Box<RenameMany>),
}

/// Where the rename dialog's parts are. See [`App::rename_layout`].
pub struct RenameLayout {
    pub find: Rect,
    pub replace: Rect,
    pub headings: Rect,
    pub list: Rect,
    pub summary: Rect,
    pub row_h: i32,
}

/// One dialog button: its label and the key that presses it.
pub struct DialogButton {
    pub label: &'static str,
    pub key: &'static str,
    pub danger: bool,
}

/// A part of the window that scrolls when what's in it doesn't fit: the list of names, or the
/// sidebar's places. Both scroll by the same rules and show the same scroll bar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pane {
    List,
    Sidebar,
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
    /// A drive's eject button in the sidebar.
    Eject(usize),
    /// Between pins, where a drag of folders would pin them: in front of the pin this far down
    /// the pins, or after the last. Only a drop target, never under the pointer otherwise.
    PinGap(usize),
    Header(SortKey),
    Row(usize),
    ListEmpty,
    Scrollbar(Pane),
    NoticeClose(usize),
    MenuItem(usize),
    Outside,
    JobCancel(usize),
    DialogButton(usize),
    DialogCheck,
    /// One of the rename dialog's fields: Find or Replace.
    DialogField(Focus),
    PreviewBody,
    /// A chooser's bar: its name field, file-type list and buttons.
    ChooserName,
    ChooserFilter,
    ChooserAccept,
    ChooserCancel,
}

/// How far the pointer has to travel before a press on a row becomes a drag, in logical pixels.
const DRAG_START: f64 = 6.0;
/// How long pointer events can keep arriving after a drag is handed to the compositor.
const WAYLAND_DRAG_GRACE: Duration = Duration::from_millis(300);
/// How near the top or bottom of a pin a dragged folder has to be to go between pins rather than
/// into that pin's folder, in logical pixels. A sidebar row is 30.
const PIN_EDGE: f64 = 7.0;
/// A row's thumbnail, square, in logical pixels: a little bigger than the 16-pixel icon it
/// replaces, in a 26-pixel row.
pub(crate) const THUMBNAIL_PX: f64 = 20.0;

/// A pin pressed in the sidebar. It opens on the release, unless the pointer moves far enough
/// first, and then it's carried up or down the pins to put them in another order.
pub(crate) struct PinDrag {
    at: (i32, i32),
    /// Which pin, by its folder, so another window changing the pins can't swap it for another.
    path: PathBuf,
    pub(crate) active: bool,
    /// Where it would land among the pins, if that moves it.
    pub(crate) gap: Option<usize>,
}

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
/// How near the sidebar's top or bottom a drag has to be to scroll it, in logical pixels.
const SIDE_SCROLL_EDGE: f64 = 20.0;

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
    /// Everything carried is a folder, which can be pinned. Not from the Trash.
    folders: bool,
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
    /// It carries a picture, as a browser does. That's saved as a new file, so it can't go to
    /// the Trash, which doesn't light up for it.
    picture: bool,
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
    /// Where the folders would be pinned, for a drop between pins. `dest` means nothing then.
    pin: Option<Spot>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DropAction {
    Move,
    Copy,
    Link,
    Trash,
    Pin,
}

#[derive(Clone, Debug, PartialEq, Eq, Copy)]
pub enum Action {
    Open,
    /// Opens the files selected with an app picked from a list.
    OpenWith,
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
    Redo,
    Restore,
    EmptyTrash,
    /// Goes to the folder a recent file is in, with it selected.
    ShowInFolder,
    /// Makes the one picture selected the desktop background.
    SetBackground,
    /// Unpacks the archives selected, each into a folder beside it.
    Extract,
    /// Opens the person's terminal in the folder selected, or the one on screen.
    OpenTerminal,
    /// For the pin whose menu it is.
    OpenPin,
    Unpin,
    /// For the drive whose menu it is.
    OpenDrive,
    Unmount,
    Eject,
    /// In a chooser, shows the files of the filter at this place in its list.
    ChooseFilter(usize),
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
    /// The order each folder was last sorted in.
    pub folder_sorts: FolderSorts,
    pub show_hidden: bool,
    pub filter: String,
    /// The filter reaches into the folders below as well, as a search.
    pub search_below: bool,
    pub loading: bool,
    pub load_error: Option<String>,
    load: Option<LoadHandle>,
    load_gen: u64,
    derive_gen: u64,
    derive_cancel: Arc<AtomicBool>,
    req_hidden: bool,
    req_filter: String,
    /// What the load under way searches for, if it's a search.
    req_search: Option<String>,
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
    /// How far the sidebar is scrolled, when its places don't fit.
    pub side_scroll: f64,
    /// When a drag held near the sidebar's top or bottom edge scrolls it next.
    side_scroll_at: Option<Instant>,

    pub history: History,
    cache: Vec<Cached>,
    pub places: Vec<Place>,
    /// The places that aren't disks, which stay as they are when drives come and go.
    standard_places: Vec<Place>,
    /// Where the system bus is, for udisks2. `None` until drives are being watched.
    pub system_bus: Option<String>,
    /// Where the session bus is, for the desktop portal. `None` until it's being watched.
    pub session_bus: Option<String>,
    /// Whether the desktop portal can set the background, which "Set as background" needs.
    pub can_set_background: bool,
    /// How to open a terminal, where `xdg-terminal-exec` is installed.
    pub terminal: Option<gannet_core::terminal::Terminal>,
    /// The drives udisks2 reported last.
    pub volumes: Vec<Volume>,
    /// The drive a sidebar menu is open for.
    drive_menu: Option<Volume>,
    /// GTK's bookmarks file, where pins are kept. `None` in a window with no sidebar to pin to.
    pub bookmarks: Option<PathBuf>,
    /// The pinned folders, in order. They follow the standard places in the sidebar.
    pins: Vec<Place>,
    /// The pin a sidebar menu is open for, by its folder.
    pin_menu: Option<PathBuf>,
    pub(crate) pin_drag: Option<PinDrag>,
    /// The drive whose eject button was pressed, by its block. Ejecting can't be taken back, so
    /// it happens on the release, and only if the pointer is still on that drive's button: the
    /// sidebar can change in between, putting another drive in that row.
    eject_pressed: Option<String>,
    /// Drives with something under way, so a second click doesn't start it again.
    pub drives_busy: Vec<String>,
    /// The drive being opened, and the folder the window was on then. The window only goes to
    /// the drive if it's still there, as the user may have gone elsewhere meanwhile.
    opening: Option<(String, PathBuf)>,
    pub edit: Edit,
    pub menu: Option<Menu>,
    pub notices: Vec<String>,
    /// How many notices have been posted, so a screen reader can tell which are new.
    pub notices_posted: u64,
    /// The last few jobs to end, each with whether it did all it was asked: not stopped, and
    /// without errors. A screen reader announces only those as finished.
    pub jobs_ended: Vec<(u64, bool)>,

    pub engine: Engine,
    pub trash_dir: PathBuf,
    /// The shared list of recently used files, `$XDG_DATA_HOME/recently-used.xbel`, which is
    /// also the Recent view's place in history and the sidebar.
    recent_file: PathBuf,
    pub jobs: Vec<JobView>,
    pub dialog: Option<Dialog>,
    pending_conflicts: Vec<Conflict>,
    pub undo: Vec<Undo>,
    /// What undo reversed, newest last. Anything new clears it.
    pub redo: Vec<Undo>,
    pending_select: Vec<Vec<u8>>,
    /// Goes up whenever the user changes the selection, so a finishing job can tell.
    selection_epoch: u64,
    /// Counts the changes started, each of which clears redo.
    change_epoch: u64,
    /// Which field is open: each one opened or closed, and each rename or new folder, is a new
    /// one, so a late paste or screen reader request about an earlier one is dropped.
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
    /// A scroll bar's thumb being dragged, and where in it the pointer holds it.
    drag_scroll: Option<(Pane, i32)>,
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
    pub(crate) has_keyboard: bool,
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
    /// This app is the preview window rather than a file manager.
    pub viewer: bool,
    /// The order was given, by the list or by `--sort`, so going to a folder doesn't look up the
    /// one saved for it.
    pub fixed_sort: bool,
    /// The file the other side last heard about. In the list, `Some` while a preview window is
    /// open.
    pub previewer: Option<PathBuf>,
    /// In the list: where its cursor was when it last told the window, or followed it. Only
    /// the cursor moving from there is news; the window showing something the list doesn't
    /// have, like a file its filter hides, isn't.
    pub(crate) previewer_cursor: Option<PathBuf>,
    pub(crate) previewer_gen: u64,
    /// The file-type database, for saying what any file is.
    pub kinds: Arc<preview::Kinds>,
    /// The fonts text and Markdown are set in, once the preview has shown some.
    pub doc_fonts: Option<crate::docview::Fonts>,
    /// Where other programs keep thumbnails.
    pub cache_home: Option<PathBuf>,
    pub slow_preview: Option<Duration>,
    pub(crate) key_is_repeat: bool,
    /// Set when this window is an Open or Save dialog for another program.
    pub chooser: Option<crate::chooser::Chooser>,
    /// The thumbnails shown in place of pictures' and videos' icons. `None` in the preview
    /// window, which has no list to show them in.
    pub thumbs: Option<crate::thumbs::Thumbs>,
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
            folder_sorts: FolderSorts::in_memory(),
            show_hidden: false,
            filter: String::new(),
            search_below: false,
            loading: false,
            load_error: None,
            load: None,
            load_gen: 0,
            derive_gen: 0,
            derive_cancel: Arc::new(AtomicBool::new(false)),
            req_hidden: false,
            req_filter: String::new(),
            req_search: None,
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
            side_scroll: 0.0,
            side_scroll_at: None,
            history: History::default(),
            cache: Vec::new(),
            standard_places: places.iter().filter(|p| p.kind != PlaceKind::Disk).cloned().collect(),
            places,
            system_bus: None,
            session_bus: None,
            can_set_background: false,
            terminal: None,
            volumes: Vec::new(),
            drive_menu: None,
            bookmarks: None,
            pins: Vec::new(),
            pin_menu: None,
            pin_drag: None,
            eject_pressed: None,
            drives_busy: Vec::new(),
            opening: None,
            edit: Edit::None,
            menu: None,
            notices: Vec::new(),
            notices_posted: 0,
            jobs_ended: Vec::new(),
            engine,
            recent_file: trash_dir.parent().and_then(Path::parent).map_or_else(PathBuf::new, |d| d.join(gannet_core::recent::FILE)),
            trash_dir,
            jobs: Vec::new(),
            dialog: None,
            pending_conflicts: Vec::new(),
            undo: Vec::new(),
            redo: Vec::new(),
            pending_select: Vec::new(),
            selection_epoch: 0,
            change_epoch: 0,
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
            viewer: false,
            fixed_sort: false,
            previewer: None,
            previewer_cursor: None,
            previewer_gen: 0,
            kinds: Arc::new(preview::Kinds::default()),
            doc_fonts: None,
            cache_home: None,
            slow_preview: None,
            key_is_repeat: false,
            chooser: None,
            thumbs: None,
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
            self.side_scroll *= scale / self.scale;
        }
        self.scale = scale;
        self.text.set_size(crate::text::UI_PX * scale as f32, crate::text::UI_HINTED && scale < 1.5);
        if let Some(fonts) = self.doc_fonts.as_mut() {
            fonts.set_scale(scale);
        }
        self.clamp_scroll();
        self.clamp_side_scroll();
        self.stats_range = (0, 0);
        self.request_stats();
        self.preview_rescale();
        self.invalidate_all();
    }

    pub fn layout(&self) -> Layout {
        let (w, h) = (self.w, self.h);
        let bar = self.px(44.0);
        // A chooser's bar holds a name field and buttons.
        let status = self.px(if self.chooser.is_some() { 54.0 } else { 26.0 });
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
        self.go(Stop { dir, cursor_name, search: None });
    }

    fn current_stop(&self) -> Stop {
        // With no cursor showing there's nothing to come back to.
        let cursor_name = if self.cursor_shown { self.listing.as_ref().and_then(|l| self.rows.get(self.cursor).map(|&i| l.key(i).into_owned())) } else { None };
        Stop { dir: self.dir.clone(), cursor_name, search: self.search_query() }
    }

    /// Shows a folder now if it's cached, and reads it either way.
    pub fn go(&mut self, stop: Stop) {
        self.rows_changing();
        self.dir = stop.dir;
        // A preview window opened by the list is told the list's order instead.
        if !self.fixed_sort {
            self.sort = self.folder_sorts.for_dir(&self.dir);
        }
        self.pending_cursor = stop.cursor_name;
        // Back to a search searches again.
        self.search_below = stop.search.is_some();
        self.filter = stop.search.unwrap_or_default();
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
        // A chooser keeps the title it was asked for.
        if self.chooser.is_none() {
            self.cmds.push(Cmd::Title(self.crumb_label(&self.dir)));
        }
        self.show_folder();
    }

    /// Shows the folder now if it's cached and there's no search, and reads or searches it
    /// either way.
    fn show_folder(&mut self) {
        // The full path picking a search's match put in a Save dialog's name would still save
        // over that file once the folder showing is another: only the name stays. A path the
        // user typed is left alone.
        if let Some(c) = self.chooser.as_mut()
            && let Some(path) = c.match_path.take()
            && c.name.text == path
            && let Some(name) = Path::new(&path).file_name().map(|n| n.to_string_lossy().into_owned())
        {
            c.name = Field::new(&name);
        }
        match self.cache.iter().position(|c| c.listing.dir == self.dir).filter(|_| self.search_query().is_none()) {
            Some(i) => {
                let c = self.cache.remove(i);
                self.show_cached(c);
            }
            None => {
                self.listing = None;
                self.sorted = Arc::new(Vec::new());
                self.rows.clear();
                self.stater = None;
                self.hold_paint = Some(Instant::now() + HOLD_FOR_LOAD);
            }
        }
        self.start_load();
        self.invalidate_all();
    }

    fn show_cached(&mut self, c: Cached) {
        let same_sort = c.sort == self.view_sort();
        self.listing = Some(c.listing.clone());
        self.sorted = c.sorted;
        self.rows = self.choosable(&c.listing, visible_rows(&c.listing, &self.sorted, self.show_hidden, &self.filter));
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
        self.req_search = self.search_query();
        let row_h = self.layout().row_h.max(1);
        let req = LoadRequest {
            dir: self.dir.clone(),
            generation: self.load_gen,
            sort: self.view_sort(),
            show_hidden: self.show_hidden,
            filter: self.filter.clone(),
            first_row: (self.scroll / row_h as f64) as usize,
            row_count: self.row_capacity(),
            slow_read: self.slow_read,
            slow_stat: None,
            trash: self.is_trash_view().then(|| TrashSpec { home_trash: self.home_trash(), uid: rustix::process::geteuid().as_raw(), mounts: self.trash_mounts.clone(), disks_give_up: DISKS_GIVE_UP }),
            recent: self.is_recent_view(),
            search: self.req_search.clone(),
            search_skips_trash: Some((self.home_trash(), rustix::process::geteuid().as_raw())),
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
        self.side_scroll_on_timer(now);
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
        // A clash's buttons can be used once it has been up a moment, and a screen reader is told.
        let guard = self.dialog_guard.map(|t| t + DIALOG_GUARD).filter(|&t| t > Instant::now() && self.dialog.is_some());
        [self.reload_at, self.hold_paint, job, self.preview_deadline(), band, self.side_scroll_at, guard].into_iter().flatten().min()
    }

    fn on_loaded(&mut self, l: Loaded) {
        if l.generation != self.load_gen {
            return;
        }
        self.hold_paint = None;
        self.first_load = false;
        // A search of a folder is another view of it, and so is a search for something else.
        let same_dir = self.listing.as_ref().is_some_and(|o| o.dir == l.listing.dir && o.search_query() == l.listing.search_query());
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
        self.rows = self.choosable(&l.listing, l.rows);
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
        // In a search it's known by its path, as another match can have its name.
        let renaming_gone = match &self.edit {
            Edit::Rename { path, .. } if l.listing.search().is_some() => !(0..l.listing.len() as u32).any(|i| l.listing.path(i) == *path),
            Edit::Rename { name, .. } => l.listing.find(name).is_none(),
            _ => false,
        };
        if l.settled && renaming_gone {
            self.set_edit(Edit::None);
        }
        // Settings changed while this was loading: it was built for the old ones.
        if l.sort != self.view_sort() {
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
        // What a search found isn't what's in the folder.
        if !listing.complete || listing.search().is_some() {
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
            let rows = self.choosable(&l, visible_rows(&l, &self.sorted, self.show_hidden, &self.filter));
            self.apply_rows(rows);
            return;
        }
        let (hub, generation, sort, hidden, filter, cancel, sorted) = (self.hub.clone(), self.derive_gen, self.view_sort(), self.show_hidden, self.filter.clone(), self.derive_cancel.clone(), self.sorted.clone());
        let choosing = self.chooser.as_ref().map(|c| c.rows.clone());
        let spawned = std::thread::Builder::new().name("gannet-derive".into()).spawn(move || {
            let sorted = if resort { Arc::new(sort_entries(&l, sort, &cancel)) } else { sorted };
            let rows = visible_rows(&l, &sorted, hidden, &filter);
            let rows = match &choosing {
                Some(only) => only.keep_rows(&l, rows),
                None => rows,
            };
            if !cancel.load(Ordering::Relaxed) {
                hub.send(Msg::Derived { generation, listing: l, sorted, rows, sort });
            }
        });
        if spawned.is_err() {
            self.notice("Couldn't start a thread to sort this folder".into());
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
        self.preview_follow();
        // A reload can move the rows under a drag, and a disk can have just answered.
        self.refresh_file_drag();
        self.refresh_incoming();
        self.arm_side_scroll();
    }

    fn handle_msg(&mut self, m: Msg) {
        match m {
            Msg::Preview(e) => self.on_preview(e),
            Msg::PreviewStepped { generation, path } => self.on_preview_stepped(generation, path),
            Msg::PreviewEnded { generation, crashed } => self.on_preview_ended(generation, crashed),
            Msg::PreviewShow(path, sort) => self.on_preview_show(path, sort),
            Msg::ListGone => self.cmds.push(Cmd::Quit),
            Msg::Dropped(d) => self.on_dropped(d),
            Msg::PinDropped { folders, at, not_folders } => {
                if not_folders > 0 {
                    self.notice("Only folders can be pinned".into());
                }
                if !folders.is_empty() {
                    self.pin_folders(folders, at);
                }
            }
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
                            return self.go(Stop { dir: parent.to_path_buf(), cursor_name: Some(name.as_bytes().to_vec()), search: None });
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
                    self.request_thumbnails();
                    self.invalidate_all();
                }
            }
            Msg::Stats(g) => {
                if g == self.stater_gen {
                    self.invalidate(self.layout().list);
                    self.invalidate(self.layout().status);
                    // A row's time can show it's changed since its thumbnail was made.
                    self.request_thumbnails();
                }
            }
            Msg::Thumbnail(done) => {
                let path = done.path.clone();
                if self.thumbs.as_mut().is_some_and(|t| t.on_done(done)) {
                    self.invalidate_row_of(&path);
                }
                // One that was stopped may be back on screen.
                self.request_thumbnails();
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
                let for_documents = self.doc_fonts.as_mut().is_some_and(|f| f.add_fallback(&path, index));
                if self.text.add_font(&path, index) | for_documents {
                    self.invalidate_all();
                }
            }
            Msg::Volumes(volumes) => self.set_volumes(volumes, true),
            Msg::NoUdisks => self.set_volumes(Vec::new(), false),
            Msg::A11y(act) => self.a11y_act(act),
            Msg::DiskDone { volume, doing, result } => self.drive_done(volume, doing, result),
            Msg::DocFont(role, path, index) => {
                if self.doc_fonts.as_mut().is_some_and(|f| f.set_role(role, &path, index)) {
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
            Msg::CanSetBackground(can) => self.can_set_background = can,
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
                    self.field_edited();
                }
            }
        }
    }

    pub fn notice(&mut self, text: String) {
        self.notices_posted += 1;
        self.notices.push(text);
        self.clamp_scroll();
        self.invalidate_all();
    }

    fn field_mut(&mut self) -> Option<&mut Field> {
        match &mut self.edit {
            Edit::Path(f) | Edit::Filter(f) | Edit::NewFolder(f) | Edit::Rename { field: f, .. } => Some(f),
            // The rename dialog's field with the keyboard.
            Edit::None if matches!(self.dialog, Some(Dialog::RenameMany(_))) => match &mut self.dialog {
                Some(Dialog::RenameMany(d)) => d.field_mut(),
                _ => None,
            },
            // A chooser's name field, when it has the keyboard.
            Edit::None => self.chooser.as_mut().filter(|c| c.name_focused).map(|c| &mut c.name),
        }
    }

    pub fn set_colors(&mut self, c: Colors, source: Source) {
        self.theme_source = source;
        if c != self.colors {
            self.colors = c;
            self.text.set_contrast(c.dark);
            if let Some(fonts) = self.doc_fonts.as_mut() {
                fonts.set_contrast(c.dark);
            }
            self.invalidate_all();
        }
    }

    /// The rows on screen that could have a thumbnail are asked for, and any others waiting for
    /// one are dropped. Not in the Trash, whose files have to be restored first.
    pub fn request_thumbnails(&mut self) {
        if self.thumbs.is_none() {
            return;
        }
        let side = self.px(THUMBNAIL_PX).max(1) as u32;
        let mut visible = Vec::new();
        if let Some(l) = self.listing.as_ref().filter(|l| !l.is_trash()) {
            let first = (self.scroll / self.layout().row_h.max(1) as f64) as usize;
            for &id in self.rows.iter().skip(first).take(self.row_capacity()) {
                let kind = l.kind(id);
                let Some(media) = crate::thumbs::media(kind, l.name(id)) else { continue };
                // A link's own time isn't its file's.
                let mtime = match l.stat(id) {
                    Stat::Known { mtime, .. } if kind == Kind::File => Some(mtime),
                    _ => None,
                };
                visible.push((l.path(id), media, mtime));
            }
        }
        if let Some(t) = self.thumbs.as_mut() {
            t.want(&visible, side);
        }
    }

    /// Draws again the row for this file, if it's on screen.
    fn invalidate_row_of(&mut self, path: &Path) {
        let Some(l) = self.listing.clone() else { return };
        let first = (self.scroll / self.layout().row_h.max(1) as f64) as usize;
        let found = self.rows.iter().enumerate().skip(first).take(self.row_capacity()).find(|&(_, &id)| l.path(id) == path).map(|(row, _)| row);
        if let Some(row) = found {
            self.invalidate_row(row);
        }
    }

    /// The picture drawn for a row, if it has one.
    pub fn thumbnail(&self, l: &Listing, id: u32) -> Option<&Arc<crate::thumbs::Pic>> {
        crate::thumbs::media(l.kind(id), l.name(id))?;
        if l.is_trash() {
            return None;
        }
        self.thumbs.as_ref()?.picture(&l.path(id))
    }

    /// Asks for sizes and times of the rows on screen, and a screenful around them.
    pub fn request_stats(&mut self) {
        self.request_thumbnails();
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

    /// The selection, one bit for each listing entry.
    pub(crate) fn selected_bits(&self) -> &[u64] {
        &self.selected
    }

    /// Does what a screen reader asked. A request about a row in a folder the window has
    /// since left is dropped: the screen reader is about to hear of the new one.
    fn a11y_act(&mut self, act: crate::a11y::Act) {
        use crate::a11y::{Act, BACK, CHOOSER_SAVE_NAME, COLUMN_MODIFIED, COLUMN_NAME, COLUMN_SIZE, FILTER, NAME_FIELD, PATH_FIELD, STOP0};
        if matches!(act, Act::Wake) {
            return;
        }
        // As with the pointer and the keyboard, only a dialog or menu that's open can be used.
        // The screen reader was told the rest can't, and a request about it is dropped.
        if self.dialog.is_some() {
            return self.a11y_dialog_act(act);
        }
        if self.menu.is_some() {
            return self.a11y_menu_act(act);
        }
        if self.a11y_chooser_act(&act) {
            return;
        }
        let row = |app: &Self, dir: &Path, key: &[u8]| {
            let l = app.listing.as_ref().filter(|_| app.dir == dir)?;
            app.rows.iter().position(|&e| *l.key(e) == *key)
        };
        // Pressing something, or moving to a row, ends an edit as clicking there would. A request
        // about a row of a folder the window has left is dropped, and leaves the edit alone.
        let clicks = match &act {
            Act::Press(_) | Act::OpenPlace(_) | Act::OpenCrumb(..) | Act::Eject(..) | Act::Dismiss(..) => true,
            Act::FocusRow(dir, key) | Act::SelectRow(dir, key, _) => row(self, dir, key).is_some(),
            _ => false,
        };
        if clicks {
            self.leave_field();
        }
        match act {
            Act::Focus(FILTER) => {
                if !matches!(self.edit, Edit::Filter(_)) && self.filter_rect().is_some() {
                    self.leave_field();
                    self.start_filter("");
                }
            }
            // The other fields show only while they have the keyboard.
            Act::Focus(_) => {}
            Act::EditText(id, edit_gen, op) => {
                // A request about a field since closed, or opened again, is dropped.
                if edit_gen != self.edit_gen {
                    return;
                }
                let ours = matches!((&self.edit, id), (Edit::Path(_), PATH_FIELD) | (Edit::Filter(_), FILTER) | (Edit::Rename { .. } | Edit::NewFolder(_), NAME_FIELD)) || (id == CHOOSER_SAVE_NAME && self.a11y_name_has_keyboard());
                let saving = self.chooser.as_ref().is_some_and(|c| c.spec.mode == crate::chooser::Mode::Save);
                // Changing the text of a field that shows but isn't typed in gives it the
                // keyboard first, as clicking it does: the filter box, or the Save dialog's name.
                if !ours && id == FILTER && matches!(self.edit, Edit::None) && self.filter_rect().is_some() {
                    self.leave_field();
                    self.start_filter("");
                } else if !ours && id == CHOOSER_SAVE_NAME && saving {
                    self.leave_field();
                    if let Some(c) = self.chooser.as_mut() {
                        c.name_focused = true;
                    }
                } else if !ours {
                    return;
                }
                self.a11y_edit(op);
            }
            Act::Press(BACK) if !self.viewer => self.go_back(),
            Act::Press(crate::a11y::PREVIEW_PREVIOUS) if self.viewer => self.preview_move(-1),
            Act::Press(crate::a11y::PREVIEW_NEXT) if self.viewer => self.preview_move(1),
            Act::Press(crate::a11y::PREVIEW_CLOSE) if self.viewer => self.close_preview(),
            // A column's heading sorts by it, as clicking it does, except in Recent.
            Act::Press(id @ (COLUMN_NAME | COLUMN_SIZE | COLUMN_MODIFIED)) if !self.is_recent_view() => {
                self.action(Action::SortBy(match id {
                    COLUMN_NAME => SortKey::Name,
                    COLUMN_SIZE => SortKey::Size,
                    _ => SortKey::Modified,
                }));
            }
            // Checked by what it says, as the notices may have moved since.
            Act::Dismiss(i, text) => {
                if self.notices.get(i) == Some(&text) {
                    self.notices.remove(i);
                    self.clamp_scroll();
                    self.invalidate_all();
                }
            }
            Act::Press(id) if id >= STOP0 => {
                if let Some(j) = self.jobs.iter().find(|j| j.job.id == id - STOP0) {
                    j.job.cancel();
                    self.invalidate(self.layout().notices);
                }
            }
            Act::Press(_) | Act::Choose(..) | Act::Wake => {}
            Act::OpenCrumb(dir, i) => {
                if *dir == self.dir
                    && let Some((_, path)) = self.crumb_parts().into_iter().nth(i)
                {
                    self.open_crumb(path);
                }
            }
            // The drive is checked by name, as the sidebar may have changed since.
            Act::Eject(i, name) => {
                if let Some(v) = self.places.get(i).filter(|p| p.label == name && self.eject_rect(i).is_some()).and_then(|p| p.volume.clone()) {
                    self.drive_act(v, Doing::Eject);
                }
            }
            Act::OpenPlace(i) => {
                self.reveal_place(i);
                self.open_place(i);
            }
            Act::ShowPlace(i) => self.reveal_place(i),
            Act::FocusRow(dir, key) => {
                if let Some(at) = row(self, &dir, &key) {
                    self.move_cursor(at, Mods { ctrl: true, ..Mods::default() });
                }
            }
            Act::SelectRow(dir, key, on) => {
                if let Some(at) = row(self, &dir, &key) {
                    self.selection_epoch += 1;
                    self.set_selected(self.rows[at], on);
                    self.invalidate_list();
                }
            }
            Act::ShowRow(dir, key) => {
                if let Some(at) = row(self, &dir, &key) {
                    let l = self.layout();
                    let top = at as f64 * l.row_h as f64;
                    if top < self.scroll || top + l.row_h as f64 > self.scroll + l.list.h as f64 {
                        self.scroll_to(top);
                    }
                }
            }
        }
    }

    /// What a screen reader asked of the dialog that's open: a button pressed, which waits out a
    /// new clash as a key does, the box ticked, or the keyboard moved to a part.
    fn a11y_dialog_act(&mut self, act: crate::a11y::Act) {
        use crate::a11y::{Act, DIALOG_BUTTON0, DIALOG_CHECK, RENAME_FIND, RENAME_REPLACE};
        let guarded = self.dialog_guarded();
        let Some(d) = self.dialog.as_mut() else { return };
        let button = |id: u64| id.checked_sub(DIALOG_BUTTON0).map(|i| i as usize);
        match act {
            Act::Choose(id, name) if !guarded => {
                let Some(i) = button(id).filter(|&i| Self::dialog_buttons(d).get(i).is_some_and(|b| b.label == name)) else { return };
                self.press_dialog_button(i);
            }
            Act::Press(DIALOG_CHECK) if !guarded => {
                if let Dialog::Conflict { apply_to_all, .. } = d {
                    *apply_to_all = !*apply_to_all;
                }
            }
            Act::Focus(id) => {
                let n = Self::dialog_buttons(d).len();
                match (d, id) {
                    (Dialog::RenameMany(rm), RENAME_FIND) => rm.focus = Focus::Find,
                    (Dialog::RenameMany(rm), RENAME_REPLACE) => rm.focus = Focus::Replace,
                    (Dialog::RenameMany(rm), id) if button(id).is_some_and(|i| i < n) => rm.focus = Focus::Button(button(id).unwrap_or(0)),
                    (Dialog::Conflict { at, .. } | Dialog::ConfirmDelete { at, .. } | Dialog::ConfirmQuit { at } | Dialog::Unlock { at, .. } | Dialog::ConfirmReplace { at, .. }, id) if button(id).is_some_and(|i| i < n) => *at = button(id).unwrap_or(0),
                    _ => return,
                }
            }
            _ => return,
        }
        self.invalidate_all();
    }

    /// What a screen reader asked of the menu that's open: an item chosen, as clicking it does,
    /// or picked, as the pointer over it does.
    fn a11y_menu_act(&mut self, act: crate::a11y::Act) {
        use crate::a11y::{Act, DIALOG_BUTTON0, MENU_ITEM0};
        let Some(m) = self.menu.as_mut() else { return };
        let item = |id: u64| (MENU_ITEM0..DIALOG_BUTTON0).contains(&id).then(|| (id - MENU_ITEM0) as usize).filter(|&i| m.items.get(i).is_some_and(|i| i.enabled));
        match act {
            Act::Choose(id, name) => {
                let Some(a) = item(id).map(|i| &m.items[i]).filter(|i| i.label == name).map(|i| i.action) else { return };
                self.close_menu();
                self.action(a);
            }
            Act::Focus(id) => {
                let Some(i) = item(id) else { return };
                m.at = i;
                self.invalidate_all();
            }
            _ => {}
        }
    }

    /// A screen reader's change to the field with the keyboard, made as the keys making it
    /// would be.
    fn a11y_edit(&mut self, op: crate::a11y::TextEdit) {
        use crate::a11y::TextEdit;
        let edit_gen = self.edit_gen;
        let Some(f) = self.field_mut() else { return };
        match op {
            TextEdit::Caret(at) => f.select_chars(at, at),
            TextEdit::Select(from, to) => f.select_chars(from, to),
            TextEdit::Replace(from, to, with) => f.replace_chars(from, to, &with),
            TextEdit::Copy(from, to) | TextEdit::Cut(from, to) => {
                let (a, b) = (f.byte_of(from.min(to)), f.byte_of(from.max(to)));
                let text = f.text[a..b].to_string();
                if matches!(op, TextEdit::Cut(..)) {
                    f.replace_chars(from, to, "");
                }
                if !text.is_empty() {
                    self.copy_text(text);
                }
                if matches!(op, TextEdit::Copy(..)) {
                    return;
                }
            }
            TextEdit::Paste(at) => {
                f.select_chars(at, at);
                self.cmds.push(Cmd::ReadClipboard(Paste::IntoField { edit: edit_gen }));
            }
        }
        self.field_edited();
    }

    /// Which field is open: each opened or closed, and each rename or new folder, is a new one.
    pub fn edit_gen(&self) -> u64 {
        self.edit_gen
    }

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
        self.max_scroll_in(Pane::List)
    }

    /// Where a pane is on screen.
    pub(crate) fn pane_rect(&self, pane: Pane) -> Rect {
        let l = self.layout();
        match pane {
            Pane::List => l.list,
            Pane::Sidebar => l.sidebar,
        }
    }

    /// How tall what's in a pane is, scrolled or not.
    fn pane_content(&self, pane: Pane) -> f64 {
        match pane {
            Pane::List => self.content_height(),
            Pane::Sidebar => self.sidebar_height() as f64,
        }
    }

    pub fn pane_scroll(&self, pane: Pane) -> f64 {
        match pane {
            Pane::List => self.scroll,
            Pane::Sidebar => self.side_scroll,
        }
    }

    pub fn max_scroll_in(&self, pane: Pane) -> f64 {
        (self.pane_content(pane) - self.pane_rect(pane).h as f64).max(0.0)
    }

    fn scroll_pane_to(&mut self, pane: Pane, y: f64) {
        match pane {
            Pane::List => self.scroll_to(y),
            Pane::Sidebar => self.side_scroll_to(y),
        }
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
    pub(crate) fn move_cursor(&mut self, to: usize, m: Mods) {
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
        if self.chooser.is_some() && !self.chooser_allows(a) {
            return;
        }
        match a {
            Action::Open => self.open_selection(),
            Action::Preview => self.open_preview(),
            Action::ToggleHidden => {
                self.show_hidden = !self.show_hidden;
                self.rederive(false);
                // Hidden folders are only searched when they show.
                if self.search_query().is_some() {
                    self.start_load();
                }
                self.invalidate_all();
            }
            // Recent is always newest first.
            Action::SortBy(_) if self.is_recent_view() => {}
            Action::SortBy(key) => {
                self.sort = if self.sort.key == key { SortSpec { key, descending: !self.sort.descending } } else { key.first_order() };
                // The preview window shows the list's order and keeps none of its own.
                if !self.viewer {
                    self.folder_sorts.set(&self.dir, self.sort);
                }
                // An open preview window on a file here walks the folder in the list's order.
                // One left on a file in another folder keeps that folder's order.
                // In a search, that's any file below.
                let here = |p: &Path| p.parent() == Some(self.dir.as_path()) || (self.search_query().is_some() && p.starts_with(&self.dir));
                if let Some(p) = self.previewer.clone().filter(|p| !self.viewer && here(p)) {
                    self.cmds.push(Cmd::PreviewShow(p, self.view_sort()));
                }
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
            // Recent only lists files from elsewhere: changing them is done in their folders.
            Action::Cut | Action::Paste | Action::Rename | Action::NewFolder | Action::Trash | Action::DeleteForGood if self.is_recent_view() => self.recent_is_read_only(),
            // Search results come from many folders, so there's no one folder to add to.
            Action::Paste if self.search_query().is_some() => self.notice("Clear the search to paste into this folder".into()),
            Action::NewFolder if self.search_query().is_some() => self.notice("Clear the search to make a folder here".into()),
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
            Action::OpenPin | Action::Unpin => {
                let Some(path) = self.pin_menu.take() else { return };
                if a == Action::Unpin {
                    self.change_pins(|text| bookmarks::without(text, &path));
                } else {
                    self.navigate(path, None);
                }
            }
            Action::OpenDrive | Action::Unmount | Action::Eject => {
                let Some(v) = self.drive_menu.take() else { return };
                match a {
                    Action::OpenDrive => self.open_volume(v),
                    Action::Unmount => {
                        self.drive_act(v, Doing::Unmount);
                    }
                    _ => {
                        self.drive_act(v, Doing::Eject);
                    }
                }
            }
            Action::Undo => {
                if let Some(u) = self.undo.pop() {
                    self.start_job(Request::Undo(u));
                }
            }
            Action::Redo => {
                if let Some(u) = self.redo.pop() {
                    self.start_job(Request::Redo(u));
                }
            }
            Action::Restore => self.restore_selection(),
            Action::ChooseFilter(i) => self.choose_filter(i),
            Action::ShowInFolder => self.show_in_folder(),
            Action::SetBackground => self.set_background(),
            Action::OpenTerminal => self.open_terminal(),
            Action::Extract => {
                if let Some(archives) = self.selected_archives() {
                    self.extract(archives);
                }
            }
            Action::OpenWith => {
                if let Some(files) = self.open_with_files() {
                    self.cmds.push(Cmd::OpenWith(files));
                }
            }
            Action::EmptyTrash => self.ask_empty_trash(),
        }
    }

    /// Opens `dir` with these entries selected once it's read, and the cursor on the first.
    /// Hidden ones show, as they were asked for by name; that's decided before the read
    /// starts, so they're among the rows the cursor can land on.
    pub fn open_selecting(&mut self, dir: PathBuf, names: Vec<Vec<u8>>) {
        if names.iter().any(|n| n.first() == Some(&b'.')) {
            self.show_hidden = true;
        }
        self.go(Stop { dir, cursor_name: names.first().cloned(), search: None });
        self.pending_select = names;
    }

    /// The Trash itself, listing every Trash folder, rather than a folder inside one.
    pub fn is_trash_view(&self) -> bool {
        self.dir == self.trash_dir
    }

    /// The Recent view, listing recently used files from wherever they are.
    pub fn is_recent_view(&self) -> bool {
        self.dir == self.recent_file
    }

    /// The order rows are in: the one chosen, except in Recent, which is always newest first.
    pub fn view_sort(&self) -> SortSpec {
        if self.is_recent_view() { SortSpec { key: SortKey::Modified, descending: true } } else { self.sort }
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
                None => self.notice(format!("“{}” can't be restored: the Trash has no record of where it came from", String::from_utf8_lossy(l.name(id)))),
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
        // A chooser picks files rather than opening them.
        if self.chooser.is_some() {
            return self.chooser_open(ids);
        }
        // Opening a pile of files by accident is hard to undo.
        const MANY: usize = 20;
        if ids.len() > MANY {
            self.notice(format!("Select {MANY} items or fewer to open them at once"));
            self.invalidate_all();
            return;
        }
        let first_dir = ids.iter().copied().find(|&i| l.kind(i).is_dirlike());
        // Archives are unpacked here rather than handed to whatever app claims them.
        let archives: Vec<PathBuf> = ids.iter().filter(|&&i| is_archive(&l, i)).map(|&i| l.path(i)).collect();
        if !archives.is_empty() {
            self.extract(archives);
        }
        for &i in ids.iter().filter(|&&i| !l.kind(i).is_dirlike() && !is_archive(&l, i)) {
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
        if self.is_trash_view() || self.is_recent_view() {
            return None;
        }
        let parent = self.dir.parent()?;
        if self.trash_files_root(parent).is_some_and(|f| f == parent) {
            // The Trash view finds rows by their real path, which is where we are.
            return Some(Stop { dir: self.trash_dir.clone(), cursor_name: Some(self.dir.clone().into_os_string().into_vec()), search: None });
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
        // Each field opened or closed is a new one, so a late paste or a screen reader's request
        // about the last can't land in it.
        if e != self.edit {
            self.edit_gen += 1;
        }
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
                self.query_changed();
            }
        }
        self.invalidate(self.layout().toolbar);
        self.invalidate(self.layout().status);
    }

    /// Opens the filter on this folder only, as / does, even after a search.
    fn start_filter_here(&mut self) {
        self.start_filter("");
        self.set_search_below(false);
    }

    /// Opens the filter as a search of this folder and every folder below it, as Ctrl+F does,
    /// even after a filter. Where there's nothing below, as in the Trash, it only filters.
    fn start_search(&mut self) {
        self.start_filter("");
        self.set_search_below(true);
    }

    /// Makes the filter reach into the folders below, or only this one.
    fn set_search_below(&mut self, below: bool) {
        let below = below && self.can_search();
        if below != self.search_below {
            self.search_below = below;
            self.query_changed();
        }
    }

    /// The Trash and Recent views list what's in other folders, so they have nothing below.
    pub fn can_search(&self) -> bool {
        !self.is_trash_view() && !self.is_recent_view()
    }

    /// What the list is searching this folder and below for, if it's a search.
    pub fn search_query(&self) -> Option<String> {
        (self.search_below && !self.filter.is_empty() && self.can_search()).then(|| self.filter.clone())
    }

    /// The filter's text or reach has changed. What's showing narrows at once, and a search
    /// starts again from the top, stopping the one under way.
    fn query_changed(&mut self) {
        let was_search = self.req_search.is_some() || self.listing.as_ref().is_some_and(|l| l.search().is_some());
        if self.search_query().is_some() {
            self.rederive(false);
            self.start_load();
        } else if was_search {
            // Back to the folder's own rows, from the cache if they're there.
            self.rows_changing();
            self.clear_selection();
            self.cursor = 0;
            self.anchor = 0;
            self.cursor_shown = false;
            self.scroll = 0.0;
            self.show_folder();
        } else {
            self.rederive(false);
        }
        self.invalidate_all();
    }

    /// Empties the filter, ending a search.
    fn clear_filter(&mut self) {
        self.search_below = false;
        if !self.filter.is_empty() {
            self.filter.clear();
            self.query_changed();
        }
    }

    fn commit_path(&mut self, text: &str) {
        let expanded = expand_home(text, self.places.first().map(|p| p.path.as_path()));
        self.set_edit(Edit::None);
        if !expanded.is_absolute() {
            self.notice("Enter a full path, starting with / or ~".to_string());
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
        let kind = match r {
            Request::Undo(_) => JobKind::Undo,
            Request::Redo(_) => JobKind::Redo,
            Request::Delete { .. } | Request::Copy { .. } | Request::Link { .. } => JobKind::EndsUndo,
            _ => JobKind::Change,
        };
        if matches!(kind, JobKind::Change | JobKind::EndsUndo) {
            self.redo.clear();
            self.change_epoch += 1;
        }
        let job = self.engine.start(r);
        self.jobs.push(JobView { job, started: Instant::now(), kind, selection_epoch: self.selection_epoch, change_epoch: self.change_epoch });
    }

    /// Goes to the folder the row under the cursor is in, or the first one selected, with it
    /// selected there.
    fn show_in_folder(&mut self) {
        let Some(l) = self.listing.clone() else { return };
        let id = self.rows.get(self.cursor).copied().filter(|&i| self.cursor_shown && (self.sel_count == 0 || self.is_selected(i))).or_else(|| self.selected_ids().first().copied());
        let Some(path) = id.map(|i| l.path(i)) else { return };
        let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else { return };
        // Back comes back to Recent, on this file.
        let from = self.current_stop();
        self.history.visit(from);
        self.open_selecting(dir.to_path_buf(), vec![name.as_encoded_bytes().to_vec()]);
    }

    fn recent_is_read_only(&mut self) {
        self.notice("Recent only lists files. Use Show in folder to change one".into());
    }

    /// Whether what's dragged from here can only leave as copies: the Trash's items, whose two
    /// folders have to stay paired, and Recent's or a search's, which list files from elsewhere.
    fn copies_only(&self) -> bool {
        self.in_trash() || self.is_recent_view() || self.search_query().is_some()
    }

    /// Recent and search results list files from many folders, so there's nowhere in the list
    /// to drop onto.
    fn list_takes_no_drops(&self) -> bool {
        self.in_trash() || self.is_recent_view() || self.search_query().is_some()
    }

    pub(crate) fn in_trash(&self) -> bool {
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
            // An Open or Save dialog renames one item at a time, as its menu says, and so does a
            // search: its matches come from many folders, and the names they'd clash with are in each.
            if ids.len() > 1 && (self.chooser.is_some() || self.search_query().is_some()) {
                self.notice("Select one item to rename it".into());
            } else if ids.len() > 1 {
                // In the order they're listed, which is the order a running number counts in.
                let items = ids.iter().map(|&i| (l.path(i), l.name(i).to_vec(), l.kind(i).is_dirlike())).collect();
                let taken = (0..l.len() as u32).map(|i| l.name(i).to_vec()).collect();
                self.set_edit(Edit::None);
                self.edit_gen += 1;
                self.dialog = Some(Dialog::RenameMany(Box::new(RenameMany::new(items, taken))));
                self.invalidate_all();
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
                if self.jobs_ended.len() >= 16 {
                    self.jobs_ended.remove(0);
                }
                self.jobs_ended.push((o.job, !o.cancelled && o.errors == 0));
                let selection_untouched = view.as_ref().is_some_and(|v| v.selection_epoch == self.selection_epoch);
                if o.cancelled
                    && let Some(v) = &view
                {
                    self.notice(format!("Stopped: {}", v.job.label));
                }
                let kind = view.as_ref().map(|v| v.kind);
                let changed_since = view.as_ref().is_some_and(|v| v.change_epoch != self.change_epoch);
                if let (Some(k @ (JobKind::Undo | JobKind::Redo)), Some((done, total))) = (kind, o.reversed)
                    && done < total
                    && total > 1
                    && !o.cancelled
                {
                    let verb = if k == JobKind::Undo { "Undid" } else { "Redid" };
                    self.notice(format!("{verb} {done} of {total} changes. The others were left as they were"));
                }
                match (kind, o.undo) {
                    // Something new began while it ran, and that clears redo.
                    (Some(JobKind::Undo), Some(_)) if changed_since => {}
                    (Some(JobKind::Undo), Some(u)) => push_capped(&mut self.redo, u),
                    (_, Some(u)) => push_capped(&mut self.undo, u),
                    (Some(JobKind::EndsUndo), None) => {
                        self.undo.clear();
                        self.redo.clear();
                    }
                    _ => {}
                }
                // Another disk's Trash may not be watched, and the Trash view shows it too.
                let trash_touched = self.is_trash_view() && o.touched.iter().any(|t| self.trash_files_root(t).is_some());
                // A search shows what's in every folder below, which only this folder is watched for.
                let search_touched = self.search_query().is_some() && o.touched.iter().any(|t| t.starts_with(&self.dir));
                if o.touched.contains(&self.dir) || trash_touched || search_touched {
                    use std::os::unix::ffi::OsStrExt;
                    // A selection made since the job began is the user's, and stays.
                    // A search's rows are known by their paths, and can be anywhere below.
                    self.pending_select = if search_touched {
                        o.created.iter().filter(|_| selection_untouched).filter(|p| p.starts_with(&self.dir)).map(|p| p.as_os_str().as_bytes().to_vec()).collect()
                    } else {
                        o.created.iter().filter(|_| selection_untouched).filter(|p| p.parent() == Some(self.dir.as_path())).filter_map(|p| p.file_name()).map(|n| n.as_bytes().to_vec()).collect()
                    };
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
                    // No letter: this one sends the old item to the Trash, so it takes a deliberate choice.
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
            // No letter: replacing loses what's there, so it takes a deliberate choice.
            Dialog::ConfirmReplace { .. } => vec![b("Replace", "", true), b("Cancel", "Esc", false)],
            Dialog::Unlock { .. } => vec![b("Unlock", "", false), b("Cancel", "Esc", false)],
            Dialog::RenameMany(_) => vec![b("Rename", "", false), b("Cancel", "Esc", false)],
        }
    }

    /// What a dialog asks, and the line under it, as it's drawn and as a screen reader says it.
    pub fn dialog_text(&self, d: &Dialog) -> (String, String) {
        let name = |p: &Path| p.file_name().map_or_else(|| p.to_string_lossy().into_owned(), |n| n.to_string_lossy().into_owned());
        match d {
            Dialog::Conflict { conflict, .. } => {
                let t = if conflict.both_folders { format!("A folder named “{}” is already here", name(&conflict.target)) } else { format!("“{}” is already here", name(&conflict.target)) };
                let body = if conflict.both_folders {
                    "Merge puts the contents together and asks about any clashes."
                } else if conflict.can_replace {
                    "Replace trashes the old one. Keep both renames the new one."
                } else {
                    "One's a file and the other's a folder, so it can't be replaced."
                };
                (t, body.to_string())
            }
            Dialog::ConfirmDelete { paths, empty: true, .. } => {
                let t = match paths.len() {
                    1 => "Delete the one item in the Trash for good?".to_string(),
                    n => format!("Delete all {n} items in the Trash for good?"),
                };
                // A disk that didn't answer keeps its items, and the question shouldn't hide that.
                let more = if self.load_error.is_some() { " A disk that couldn't be read keeps its items." } else { "" };
                (t, format!("This can't be undone.{more}"))
            }
            Dialog::ConfirmDelete { paths, .. } => {
                let t = match paths.as_slice() {
                    [one] => format!("Delete “{}” for good?", name(one)),
                    many => format!("Delete {} for good?", crate::format::count(many.len(), "item", "items")),
                };
                (t, "This can't be undone.".to_string())
            }
            Dialog::ConfirmReplace { paths, existing, .. } => {
                let t = match existing.as_slice() {
                    [one] => format!("Replace “{}”?", crate::chooser::shown_name(one)),
                    many => format!("Replace {} of the {} files?", many.len(), paths.len()),
                };
                let folder = existing.first().and_then(|p| p.parent()).map_or_else(String::new, |d| self.crumb_label(d));
                let body = if existing.len() == 1 { format!("It's already in “{folder}”. Replacing it overwrites what's there.") } else { format!("They're already in “{folder}”. Replacing them overwrites what's there.") };
                (t, body)
            }
            Dialog::ConfirmQuit { .. } => ("Operations are still running".to_string(), "Closing stops them. Whatever's finished stays finished.".to_string()),
            Dialog::Unlock { volume, error, waiting, .. } => {
                let line = if *waiting { "Unlocking…".to_string() } else { error.clone().unwrap_or_else(|| "It's encrypted. Type its passphrase to open it.".to_string()) };
                (format!("Unlock “{}”", Self::drive_name(volume)), line)
            }
            Dialog::RenameMany(d) => (d.title(), d.summary().0),
        }
    }

    /// The button Enter presses: the one picked, or in the rename dialog, Rename while a field
    /// has the keyboard.
    pub fn dialog_default(d: &Dialog) -> usize {
        match d {
            Dialog::Conflict { at, .. } | Dialog::ConfirmDelete { at, .. } | Dialog::ConfirmQuit { at } | Dialog::Unlock { at, .. } | Dialog::ConfirmReplace { at, .. } => *at,
            Dialog::RenameMany(d) => if let Focus::Button(i) = d.focus { i } else { 0 },
        }
    }

    /// Conflicts appear on their own, so Enter means Skip and nothing is answered in the
    /// moment after one pops up.
    fn show_conflict(&mut self, conflict: Conflict) {
        let at = usize::from(conflict.can_replace);
        self.dialog = Some(Dialog::Conflict { conflict, apply_to_all: false, at });
        self.dialog_guard = Some(Instant::now());
    }

    pub(crate) fn dialog_guarded(&self) -> bool {
        self.dialog_guard.is_some_and(|t| t.elapsed() < DIALOG_GUARD)
    }

    /// When the last clash was asked.
    pub(crate) fn dialog_asked(&self) -> Option<Instant> {
        self.dialog_guard
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
            Dialog::Unlock { volume, field, error, waiting: true, at } if label == "Unlock" => {
                // Already being tried: a second press waits with the first.
                self.dialog = Some(Dialog::Unlock { volume, field, at, error, waiting: true });
                return;
            }
            Dialog::Unlock { volume, field, error, .. } => {
                if label == "Unlock" {
                    // Kept open, waiting, until udisks2 says whether it worked.
                    let busy = self.drives_busy.contains(&volume.block);
                    let waiting = !field.text.is_empty() && self.drive_act(volume.clone(), Doing::Unlock(field.text.clone()));
                    if waiting {
                        self.opening = Some((volume.block.clone(), self.dir.clone()));
                    }
                    let error = if busy { Some("Something's already under way on this drive. Try again in a moment.".to_string()) } else { error };
                    self.dialog = Some(Dialog::Unlock { volume, field, at: 0, error, waiting });
                    self.invalidate_all();
                    return;
                }
                // Cancelled: if it was already being tried, the window stays where it is.
                self.opening = None;
            }
            Dialog::ConfirmReplace { paths, .. } => {
                if label == "Replace" {
                    self.chooser_done(paths);
                }
            }
            Dialog::RenameMany(d) => {
                if label == "Rename" {
                    // The line under the names says what's stopping it.
                    if !d.can_rename() {
                        self.dialog = Some(Dialog::RenameMany(d));
                        self.invalidate_all();
                        return;
                    }
                    self.start_job(Request::RenameMany { renames: d.renames() });
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

    /// Keys in the rename dialog. Typing goes to Find or Replace, whichever has the keyboard, and
    /// Enter renames from either. Tab goes round the fields and buttons, and Up, Down, Page Up and
    /// Page Down scroll the names.
    fn rename_dialog_key(&mut self, sym: u32, utf8: &str, m: Mods) {
        let shown = self.rename_rows_shown();
        let Some(Dialog::RenameMany(d)) = self.dialog.as_mut() else { return };
        let page = shown.saturating_sub(1).max(1);
        match sym {
            k::KEY_Escape => return self.press_dialog_button(1),
            k::KEY_Return | k::KEY_KP_Enter => {
                let i = if let Focus::Button(i) = d.focus { i } else { 0 };
                return self.press_dialog_button(i);
            }
            k::KEY_Tab | k::KEY_ISO_Left_Tab => d.tab(m.shift || sym == k::KEY_ISO_Left_Tab, 2),
            k::KEY_Up => d.scroll = d.scroll.saturating_sub(1),
            k::KEY_Down => d.scroll += 1,
            k::KEY_Page_Up => d.scroll = d.scroll.saturating_sub(page),
            k::KEY_Page_Down => d.scroll += page,
            k::KEY_Left | k::KEY_Right if matches!(d.focus, Focus::Button(_)) => d.focus = Focus::Button(usize::from(sym == k::KEY_Right)),
            k::KEY_v if m.ctrl && d.field_mut().is_some() => {
                self.cmds.push(Cmd::ReadClipboard(Paste::IntoField { edit: self.edit_gen }));
                return;
            }
            _ => {
                let Some(f) = d.field_mut() else { return };
                match sym {
                    k::KEY_Left => f.left(m.ctrl, m.shift),
                    k::KEY_Right => f.right(m.ctrl, m.shift),
                    k::KEY_Home => f.move_to(0, m.shift),
                    k::KEY_End => f.move_to(f.text.len(), m.shift),
                    k::KEY_BackSpace => f.backspace(m.ctrl),
                    k::KEY_Delete => f.delete(m.ctrl),
                    k::KEY_a if m.ctrl => f.select_all(),
                    _ if !m.ctrl && !m.alt && !m.logo && !utf8.is_empty() && !utf8.chars().any(char::is_control) => f.insert(utf8),
                    _ => return,
                }
                d.update();
            }
        }
        d.clamp_scroll(shown);
        self.invalidate_all();
    }

    fn dialog_key(&mut self, sym: u32, utf8: &str, m: Mods) {
        if self.dialog_guarded() {
            return;
        }
        if matches!(self.dialog, Some(Dialog::RenameMany(_))) {
            return self.rename_dialog_key(sym, utf8, m);
        }
        let Some(d) = self.dialog.as_mut() else { return };
        let buttons = Self::dialog_buttons(d);
        let n = buttons.len();
        if let Dialog::Unlock { field, waiting, .. } = d {
            // Typing goes to the passphrase, and it's left alone while it's being tried.
            let edited = !*waiting && match sym {
                k::KEY_BackSpace => {
                    field.backspace(false);
                    true
                }
                k::KEY_Delete => {
                    field.delete(false);
                    true
                }
                k::KEY_Left => {
                    field.left(false, false);
                    true
                }
                k::KEY_Right => {
                    field.right(false, false);
                    true
                }
                k::KEY_Home => {
                    field.move_to(0, false);
                    true
                }
                k::KEY_End => {
                    field.move_to(field.text.len(), false);
                    true
                }
                k::KEY_Return | k::KEY_KP_Enter | k::KEY_Escape | k::KEY_Tab => false,
                _ if !utf8.is_empty() && !utf8.chars().any(char::is_control) => {
                    field.insert(utf8);
                    true
                }
                _ => false,
            };
            if edited || (*waiting && sym != k::KEY_Escape) {
                return self.invalidate_all();
            }
        }
        let at = match d {
            Dialog::Conflict { at, .. } | Dialog::ConfirmDelete { at, .. } | Dialog::ConfirmQuit { at } | Dialog::Unlock { at, .. } | Dialog::ConfirmReplace { at, .. } => at,
            // Has its own keys, above.
            Dialog::RenameMany(_) => return,
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

    /// Where a dialog goes. The rename dialog is bigger, to list the names.
    pub fn dialog_rect_for(&self, d: &Dialog) -> Rect {
        match d {
            Dialog::RenameMany(rm) => self.rename_rect(rm.names.len()),
            _ => self.dialog_rect(),
        }
    }

    /// The rename dialog's size: tall enough for `rows` names, from four up to as many as fit.
    pub(crate) fn rename_rect(&self, rows: usize) -> Rect {
        // Everything but the names: title, fields, headings, the line under the names, buttons.
        let fixed = self.px(228.0);
        let list = self.px(24.0) * rows.clamp(4, 40) as i32;
        let (w, h) = (self.px(680.0).min(self.w - self.px(24.0)).max(1), (fixed + list).min(self.px(560.0)).min(self.h - self.px(24.0)).max(1));
        Rect::new((self.w - w) / 2, (self.h - h) / 2, w, h)
    }

    /// How many names the open rename dialog has, or none.
    fn rename_count(&self) -> usize {
        match &self.dialog {
            Some(Dialog::RenameMany(d)) => d.names.len(),
            _ => 0,
        }
    }

    /// The rename dialog's parts, for `rows` names: the Find and Replace fields, the names'
    /// column headings, the list of names and the line under it. The fields leave room on the
    /// left for their labels.
    pub fn rename_layout(&self, rows: usize) -> RenameLayout {
        let r = self.rename_rect(rows);
        let pad = self.px(16.0);
        let label = self.px(104.0);
        let field_h = self.px(30.0);
        let top = r.y + self.px(12.0) + self.px(26.0) + self.px(8.0);
        let find = Rect::new(r.x + pad + label, top, r.w - 2 * pad - label, field_h);
        let replace = Rect::new(find.x, find.bottom() + self.px(8.0), find.w, field_h);
        let headings = Rect::new(r.x + pad, replace.bottom() + self.px(12.0), r.w - 2 * pad, self.px(22.0));
        let summary_h = self.px(26.0);
        let buttons_top = r.bottom() - self.px(12.0) - self.px(30.0);
        let summary = Rect::new(r.x + pad, buttons_top - self.px(8.0) - summary_h, r.w - 2 * pad, summary_h);
        let list = Rect::new(r.x + pad, headings.bottom(), r.w - 2 * pad, (summary.y - self.px(4.0) - headings.bottom()).max(0));
        RenameLayout { find, replace, headings, list, summary, row_h: self.px(24.0).max(1) }
    }

    /// How many names the open rename dialog's list has room for.
    pub fn rename_rows_shown(&self) -> usize {
        let l = self.rename_layout(self.rename_count());
        (l.list.h / l.row_h).max(1) as usize
    }

    /// The button rectangles, right-aligned along the dialog's bottom.
    pub fn dialog_button_rects(&mut self, d: &Dialog) -> Vec<Rect> {
        let r = self.dialog_rect_for(d);
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
        // Closing a chooser is cancelling it. What it can start, a rename, a new folder or a
        // move to the Trash, is mostly quick, and one cut short leaves the originals in place.
        if self.chooser.is_some() {
            self.chooser_cancel();
        } else if self.jobs.is_empty() || self.viewer {
            self.cmds.push(Cmd::Quit);
        } else if self.dialog.is_none() {
            self.dialog = Some(Dialog::ConfirmQuit { at: 1 });
            self.invalidate_all();
        }
    }

    // ---- Drives ----

    /// Opens a place: a drive that isn't mounted is mounted first, and a locked one asks for
    /// its passphrase.
    fn open_place(&mut self, i: usize) {
        let Some(p) = self.places.get(i) else { return };
        match p.volume.clone() {
            Some(v) => self.open_volume(v),
            None => {
                let path = p.path.clone();
                self.navigate(path, None);
            }
        }
    }

    fn open_volume(&mut self, v: Volume) {
        if let Some(at) = v.mount.clone() {
            return self.navigate(at, None);
        }
        if v.locked {
            // Another question, like a copy's, comes first.
            if self.dialog.is_none() {
                self.dialog = Some(Dialog::Unlock { volume: v, field: Field::new(""), at: 0, error: None, waiting: false });
                self.invalidate_all();
            }
        } else if self.drive_act(v.clone(), Doing::Mount) {
            self.opening = Some((v.block, self.dir.clone()));
        }
    }

    /// Starts something on a drive, unless something's already under way on it. Returns whether
    /// it started.
    fn drive_act(&mut self, v: Volume, doing: Doing) -> bool {
        let Some(address) = self.system_bus.clone() else {
            self.notice(format!("Couldn't {} “{}”: udisks2 isn't running", doing.verb(), Self::drive_name(&v)));
            return false;
        };
        if self.drives_busy.contains(&v.block) {
            return false;
        }
        self.drives_busy.push(v.block.clone());
        if matches!(doing, Doing::Unmount | Doing::Eject) {
            self.let_go_of_drive(&v, doing == Doing::Eject);
        }
        crate::drives::act(self.hub.clone(), address, v, doing);
        self.invalidate_all();
        true
    }

    /// Before a drive is unmounted, lets go of every folder the window holds open on it, as
    /// udisks2 can't unmount a folder that's open: the one showing (the window goes home), those
    /// kept for going back, and the preview window's. For an eject that's every volume on the
    /// drive.
    fn let_go_of_drive(&mut self, v: &Volume, whole_drive: bool) {
        let mounts: Vec<PathBuf> = if whole_drive { self.volumes.iter().filter(|o| o.drive == v.drive).filter_map(|o| o.mount.clone()).chain(v.mount.clone()).collect() } else { v.mount.iter().cloned().collect() };
        if mounts.is_empty() {
            return;
        }
        let on = |p: &Path| mounts.iter().any(|m| p.starts_with(m));
        // Each listing says which folders it holds, including those reached through a symlink
        // and the Trash view's Trash folders on other drives.
        let held_on = |l: &Listing| l.held_dirs().iter().any(|d| on(d));
        self.cache.retain(|c| !held_on(&c.listing));
        // Until the window's folder is read, where it really is isn't known, so it goes home
        // anyway: once read, it would hold the folder open.
        let here = on(&self.dir) || self.listing.as_ref().filter(|l| l.dir == self.dir).map_or(self.loading, |l| held_on(l));
        // The preview window reads the folder of the file it shows, which can be on the drive
        // through a link from anywhere, and it can't be asked where that really is. It's quick to
        // open again.
        if self.previewer.is_some() {
            self.close_preview();
        }
        if here && let Some(home) = self.standard_places.first().map(|p| p.path.clone()) {
            self.navigate(home, None);
        }
    }

    /// Closes the dialog, and shows the next question a job is waiting on, if there is one.
    fn close_dialog(&mut self) {
        self.dialog = None;
        if !self.pending_conflicts.is_empty() {
            let c = self.pending_conflicts.remove(0);
            self.show_conflict(c);
        }
        self.invalidate_all();
    }

    fn open_drive_menu(&mut self, v: Volume, x: i32, y: i32) {
        let items = vec![
            MenuItem { label: if v.locked { "Unlock" } else { "Open" }, keys: "", action: Action::OpenDrive, enabled: true },
            // Unmounting what's inside an encrypted volume locks it, and one that's unlocked but
            // not mounted can still be locked.
            MenuItem { label: if v.unlocked && v.mount.is_none() { "Lock" } else { "Unmount" }, keys: "", action: Action::Unmount, enabled: v.mount.is_some() || v.unlocked },
            MenuItem { label: "Eject", keys: "", action: Action::Eject, enabled: v.removable },
        ];
        self.drive_menu = Some(v);
        self.menu = Some(Menu { items, x, y, at: usize::MAX });
        self.invalidate_all();
    }

    /// What a drive's called in the sidebar and in messages.
    pub fn drive_name(v: &Volume) -> String {
        v.label.clone().unwrap_or_else(|| format!("{} Volume", crate::format::size(v.size)))
    }

    /// The drives udisks2 reports now. A folder on one that's gone, or no longer mounted, can't be
    /// shown, so the window goes home, when `leave` says the list is udisks2's word on it. A drive
    /// pulled out can still be in the mount table for a moment, so that isn't asked.
    fn set_volumes(&mut self, volumes: Vec<Volume>, leave: bool) {
        let mounts = gannet_core::mounts::read_mounts();
        let gone = self.volumes.iter().filter_map(|v| v.mount.clone()).filter(|_| leave).find(|m| self.dir.starts_with(m) && !volumes.iter().any(|v| v.mount.as_ref() == Some(m)));
        // The drive whose passphrase is being asked for has gone.
        if let Some(Dialog::Unlock { volume, waiting: false, .. }) = &self.dialog
            && !volumes.iter().any(|v| v.block == volume.block)
        {
            self.close_dialog();
        }
        let mut places = self.standard_places.clone();
        places.extend(self.pins.iter().cloned());
        places.extend(gannet_core::places::all_disk_places(&volumes, &mounts, Self::drive_name));
        // While udisks2 is away, what it last said is kept, so a drive pulled out meanwhile is
        // noticed when it's back.
        if leave {
            self.volumes = volumes;
        }
        if let Some(gone) = gone {
            self.places = places;
            self.clamp_side_scroll();
            return self.leave_drive(&gone);
        }
        // udisks2 says plenty that doesn't change the sidebar.
        if places != self.places {
            self.places = places;
            self.clamp_side_scroll();
            self.invalidate_all();
        }
    }

    /// The window was on a drive that's gone: it goes home.
    fn leave_drive(&mut self, mount: &Path) {
        if !self.dir.starts_with(mount) {
            return;
        }
        if let Some(home) = self.standard_places.first().map(|p| p.path.clone()) {
            self.navigate(home, None);
        }
    }

    fn drive_done(&mut self, volume: Volume, doing: Doing, result: Result<Option<PathBuf>, DiskError>) {
        self.drives_busy.retain(|b| *b != volume.block);
        let name = Self::drive_name(&volume);
        let unlocking = matches!(&self.dialog, Some(Dialog::Unlock { volume: v, .. }) if v.block == volume.block);
        // Only what the user's still waiting for takes the window there.
        let wanted = self.opening.as_ref().is_some_and(|(block, dir)| *block == volume.block && *dir == self.dir);
        if matches!(doing, Doing::Mount | Doing::Unlock(_)) && self.opening.as_ref().is_some_and(|(block, _)| *block == volume.block) {
            self.opening = None;
        }
        let unlocking = unlocking && matches!(doing, Doing::Unlock(_));
        match (&doing, result) {
            (Doing::Unlock(_), Err(DiskError::WrongPassphrase)) if unlocking => {
                if let Some(Dialog::Unlock { field, error, waiting, .. }) = self.dialog.as_mut() {
                    *field = Field::new("");
                    *error = Some("That passphrase isn't right. Try again.".to_string());
                    *waiting = false;
                }
            }
            // Cancelled while it was tried: a wrong passphrase isn't news any more.
            (Doing::Unlock(_), Err(DiskError::WrongPassphrase)) => {}
            // The user said no to polkit, and knows.
            (_, Err(DiskError::Cancelled)) => {
                if unlocking {
                    self.close_dialog();
                }
            }
            (_, Err(e)) => {
                if unlocking {
                    self.close_dialog();
                }
                // Unmounting what's inside a locked-again volume that isn't mounted is locking it.
                let verb = if doing == Doing::Unmount && volume.unlocked && volume.mount.is_none() { "lock" } else { doing.verb() };
                self.notice(format!("Couldn't {verb} “{name}”: {e}"));
            }
            (Doing::Mount | Doing::Unlock(_), Ok(at)) => {
                if unlocking {
                    self.close_dialog();
                }
                if let Some(at) = at.filter(|_| wanted) {
                    self.navigate(at, None);
                }
            }
            (Doing::Unmount | Doing::Eject, Ok(_)) => {
                if let Some(m) = volume.mount.as_ref() {
                    self.leave_drive(m);
                }
            }
        }
        self.invalidate_all();
    }

    // ---- Pins ----

    /// Pins are kept in GTK's bookmarks file at `file`, which is read now and whenever
    /// `bookmarks_changed` says it changed.
    pub fn set_bookmarks(&mut self, file: PathBuf) {
        self.bookmarks = Some(file);
        self.bookmarks_changed();
    }

    /// Reads the pins again, as this window or another program has changed them.
    pub fn bookmarks_changed(&mut self) {
        let Some(file) = &self.bookmarks else { return };
        // A standard place pinned too, as some desktops do for GTK's dialogs, shows once.
        let mut pins = gannet_core::places::pinned_places(&bookmarks::read(file));
        pins.retain(|p| !self.standard_places.iter().any(|s| s.path == p.path));
        if pins == self.pins {
            return;
        }
        self.pins = pins;
        let mut places = self.standard_places.clone();
        places.extend(self.pins.iter().cloned());
        places.extend(self.places.iter().filter(|p| p.kind == PlaceKind::Disk).cloned());
        self.places = places;
        self.clamp_side_scroll();
        // A pin being carried that's gone has nothing left to move.
        // One that's still there is placed among the pins as they are now.
        match &self.pin_drag {
            Some(d) if self.pin_index(&d.path).is_none() => self.cancel_pin_drag(),
            Some(d) if d.active => {
                let (x, y) = self.pointer;
                self.pin_drag_moved(x, y);
            }
            // The row under the pointer can be another place now. A menu or dialog has its own
            // highlight, which follows the pointer only as it moves.
            _ if self.pointer.0 >= 0 && self.menu.is_none() && self.dialog.is_none() => {
                let (x, y) = self.pointer;
                let h = self.hit(x, y);
                self.set_hover(h);
            }
            _ => {}
        }
        self.invalidate_all();
    }

    /// Where the pins are in the sidebar: the first one's place, and how many there are. With
    /// none, where the first would go.
    pub fn pins_at(&self) -> (usize, usize) {
        let first = self.places.iter().position(|p| matches!(p.kind, PlaceKind::Pinned | PlaceKind::Disk)).unwrap_or(self.places.len());
        (first, self.pins.len())
    }

    /// Whether a folder already has a row in the sidebar's top sections, pinned or standard.
    fn in_sidebar(&self, path: &Path) -> bool {
        self.pin_index(path).is_some() || self.standard_places.iter().any(|p| p.path == path)
    }

    fn pin_index(&self, path: &Path) -> Option<usize> {
        self.pins.iter().position(|p| p.path == path)
    }

    /// The pin that a pin put at `gap` goes in front of.
    fn spot(&self, gap: usize) -> Spot {
        self.pins.get(gap).map_or(Spot::End, |p| Spot::Before(p.path.clone()))
    }

    /// Changes the bookmarks file, reading it first so nothing another program wrote is lost,
    /// and shows the pins it ends up with.
    fn change_pins(&mut self, change: impl FnOnce(&[u8]) -> Option<Vec<u8>>) {
        let Some(file) = self.bookmarks.clone() else { return };
        if let Err(e) = bookmarks::edit(&file, change) {
            self.notice(format!("Couldn't save the pins in {}: {}", file.display(), gannet_core::listing::describe(&e).to_lowercase()));
        }
        self.bookmarks_changed();
    }

    fn pin_folders(&mut self, mut paths: Vec<PathBuf>, at: Spot) {
        paths.retain(|p| !self.standard_places.iter().any(|s| s.path == *p));
        self.change_pins(|text| bookmarks::with_added(text, &paths, &at));
    }

    /// Where a drag of folders at `y` would pin them, if it's between places rather than on
    /// one: near the top or bottom edge of a pin, or of the place just above the pins, or in
    /// the space around the Pinned heading or below the last pin, before the next section.
    fn pin_gap_at(&self, x: i32, y: i32) -> Option<usize> {
        let l = self.layout();
        // A dialog, menu or preview has the pointer, and nothing underneath takes a drop.
        if self.bookmarks.is_none() || self.places.is_empty() || self.dialog.is_some() || self.menu.is_some() || self.preview.is_some() || !l.sidebar.contains(x, y) || self.on_side_scrollbar(x) {
            return None;
        }
        let (first, n) = self.pins_at();
        let (row, edge) = (self.px(30.0), self.px(PIN_EDGE));
        let above = if first > 0 { self.place_top(first - 1) + row - edge } else { l.sidebar.y };
        let below = if first + n < self.places.len() { self.place_top(first + n) } else { l.sidebar.bottom() };
        if n == 0 {
            return (above..below).contains(&y).then_some(0);
        }
        if (above..self.place_top(first)).contains(&y) {
            return Some(0);
        }
        for j in 0..n {
            let top = self.place_top(first + j);
            if (top..top + edge).contains(&y) {
                return Some(j);
            }
            if (top + row - edge..top + row).contains(&y) {
                return Some(j + 1);
            }
        }
        (self.place_top(first + n - 1) + row..below).contains(&y).then_some(n)
    }

    /// Where a pin carried to `y` would land among the pins: in front of the first whose middle
    /// is below it. It only moves among the pins, wherever the pointer goes.
    fn reorder_gap_at(&self, y: i32) -> usize {
        let (first, n) = self.pins_at();
        let half = self.px(15.0);
        (0..n).filter(|&j| y >= self.place_top(first + j) + half).count()
    }

    /// The height of the bar that marks where pins would land, `gap` pins down.
    pub fn pin_bar_y(&self, gap: usize) -> i32 {
        let (first, n) = self.pins_at();
        let row = self.px(30.0);
        match (n, gap) {
            (0, _) if first > 0 => self.place_top(first - 1) + row + self.px(SIDEBAR_GAP) / 2,
            (0, _) => self.layout().sidebar.y + self.px(8.0) - self.side_scroll as i32,
            (_, g) if g < n => self.place_top(first + g),
            _ => self.place_top(first + n - 1) + row,
        }
    }

    /// Where the bar is drawn, if a drag of folders or a pin has a place to land among the pins.
    pub fn pin_bar(&self) -> Option<i32> {
        let gap = match (&self.pin_drag, &self.file_drag, &self.incoming) {
            (Some(d), _, _) => d.gap,
            (_, Some(FileDrag::Active(ActiveDrag { target: Some(t), .. })), _) | (_, _, Some(IncomingDrag { target: Some(t), .. })) => match t.hit {
                Hit::PinGap(g) => Some(g),
                _ => None,
            },
            _ => None,
        }?;
        Some(self.pin_bar_y(gap))
    }

    /// The pin being carried, which is drawn dimmed.
    pub fn carried_pin(&self) -> Option<&Path> {
        self.pin_drag.as_ref().filter(|d| d.active).map(|d| d.path.as_path())
    }

    fn press_pin(&mut self, i: usize, x: i32, y: i32) {
        if let Some(p) = self.places.get(i) {
            self.pin_drag = Some(PinDrag { at: (x, y), path: p.path.clone(), active: false, gap: None });
        }
    }

    /// Follows the pointer with a pin pressed or carried. Returns whether the move was its.
    fn pin_drag_moved(&mut self, x: i32, y: i32) -> bool {
        let reach = self.px(DRAG_START);
        let Some(d) = &self.pin_drag else { return false };
        let (at, from) = (d.at, self.pin_index(&d.path));
        let Some(from) = from else {
            self.cancel_pin_drag();
            return false;
        };
        if !d.active && (x - at.0).abs() < reach && (y - at.1).abs() < reach {
            return false;
        }
        let gap = Some(self.reorder_gap_at(y)).filter(|&g| g != from && g != from + 1);
        if let Some(d) = &mut self.pin_drag {
            if !d.active {
                // A drag wasn't a click, so the next press can't be the second half of a double-click.
                self.last_click = None;
            }
            d.active = true;
            d.gap = gap;
        }
        self.set_hover(Hit::None);
        self.set_cursor(Cursor::Move);
        self.invalidate(self.layout().sidebar);
        true
    }

    /// The button's up: a pin that wasn't carried opens, and one that was lands where its bar is.
    fn pin_released(&mut self) {
        let Some(d) = self.pin_drag.take() else { return };
        self.end_pin_drag();
        if !d.active {
            if self.pin_index(&d.path).is_some() {
                self.navigate(d.path, None);
            }
            return;
        }
        let Some(gap) = d.gap else { return };
        let at = self.spot(gap);
        self.change_pins(|text| bookmarks::with_moved(text, &d.path, &at));
    }

    fn end_pin_drag(&mut self) {
        self.invalidate(self.layout().sidebar);
        let (x, y) = self.pointer;
        let h = self.hit(x, y);
        self.set_hover(h);
        self.set_cursor(cursor_for(h));
    }

    fn open_pin_menu(&mut self, path: PathBuf, x: i32, y: i32) {
        let items = vec![MenuItem { label: "Open", keys: "", action: Action::OpenPin, enabled: true }, MenuItem { label: "Unpin", keys: "", action: Action::Unpin, enabled: true }];
        self.pin_menu = Some(path);
        self.menu = Some(Menu { items, x, y, at: usize::MAX });
        self.invalidate_all();
    }

    /// The sidebar's sections, as the place each starts at and its heading: Places from the
    /// top, Pinned from the first pin, and Devices from the first drive or mount, if there are any.
    pub fn sidebar_headings(&self) -> Vec<(usize, &'static str)> {
        if self.places.is_empty() {
            return Vec::new();
        }
        let mut out = vec![(0, "Places")];
        for (kind, heading) in [(PlaceKind::Pinned, "Pinned"), (PlaceKind::Disk, "Devices")] {
            out.extend(self.places.iter().position(|p| p.kind == kind).filter(|&i| i > 0).map(|i| (i, heading)));
        }
        out
    }

    /// The top of a place's row on screen, in the sidebar as it's scrolled.
    pub fn place_top(&self, i: usize) -> i32 {
        self.layout().sidebar.y + self.place_offset(i) - self.side_scroll as i32
    }

    /// How far down the sidebar's places a place's row starts, below the headings of its
    /// section and the ones before it, each after the first with a gap above it.
    fn place_offset(&self, i: usize) -> i32 {
        let headings = self.sidebar_headings().iter().filter(|(h, _)| *h <= i).count() as i32;
        let above = headings * self.px(SIDEBAR_HEADING) + (headings - 1).max(0) * self.px(SIDEBAR_GAP);
        self.px(8.0) + i as i32 * self.px(30.0) + above
    }

    /// How tall the sidebar's places are, with the same space below the last as above the first.
    fn sidebar_height(&self) -> i32 {
        match self.places.len() {
            0 => 0,
            n => self.place_offset(n - 1) + self.px(30.0) + self.px(8.0),
        }
    }

    /// The room the sidebar's scroll bar takes at its right, when the places don't fit.
    pub fn side_bar_room(&self) -> i32 {
        if self.scrollbar_visible(Pane::Sidebar) { self.px(6.0) } else { 0 }
    }

    fn on_side_scrollbar(&self, x: i32) -> bool {
        self.scrollbar_visible(Pane::Sidebar) && x >= self.layout().sidebar.right() - self.px(12.0)
    }

    fn clamp_side_scroll(&mut self) {
        self.side_scroll = self.side_scroll.clamp(0.0, self.max_scroll_in(Pane::Sidebar));
    }

    fn side_scroll_to(&mut self, y: f64) {
        let old = self.side_scroll;
        self.side_scroll = y;
        self.clamp_side_scroll();
        if self.side_scroll == old {
            return;
        }
        self.invalidate(self.layout().sidebar);
        // Whatever's under the pointer just moved: a carried pin's spot, a drag's target, or
        // the place it hovers.
        let (x, y) = self.pointer;
        if self.pin_drag.as_ref().is_some_and(|d| d.active) {
            self.pin_drag_moved(x, y);
        } else if x >= 0 && self.menu.is_none() && self.dialog.is_none() && self.drag_scroll.is_none() && self.incoming.is_none() {
            // A thumb being dragged keeps its hover, wherever the pointer drifts. A drag from
            // elsewhere has the compositor's cursor, and shows only its target.
            let h = self.hit(x, y);
            self.set_hover(h);
        }
        self.refresh_file_drag();
        self.refresh_incoming();
    }

    /// Scrolls the sidebar just far enough to show a place's row, and its heading if it's the
    /// first in its section.
    pub fn reveal_place(&mut self, i: usize) {
        if i >= self.places.len() {
            return;
        }
        let side = self.layout().sidebar;
        let heading = if self.sidebar_headings().iter().any(|&(h, _)| h == i) { self.px(SIDEBAR_HEADING) } else { 0 };
        let (top, bottom) = (self.place_offset(i) - heading - self.px(8.0), self.place_offset(i) + self.px(30.0) + self.px(8.0));
        if (top as f64) < self.side_scroll {
            self.side_scroll_to(top as f64);
        } else if bottom as f64 > self.side_scroll + side.h as f64 {
            self.side_scroll_to((bottom - side.h) as f64);
        }
    }

    /// Which way a drag held near the sidebar's top or bottom edge scrolls it, and how far each
    /// step: none, when there's no drag there or nowhere further to go.
    fn side_drag_step(&self) -> f64 {
        // A drag from elsewhere, or one of this window's out and back, has a position only once
        // it's moved: Hyprland says the middle of the window when one arrives.
        let carrying_pin = self.pin_drag.as_ref().is_some_and(|d| d.active);
        let dragging = carrying_pin || matches!(&self.file_drag, Some(FileDrag::Active(d)) if !d.wayland || (d.inside && d.moved_inside)) || self.incoming.as_ref().is_some_and(|i| i.moved);
        if !dragging || self.dialog.is_some() || self.menu.is_some() || self.preview.is_some() {
            return 0.0;
        }
        let side = self.layout().sidebar;
        let (x, y) = self.pointer;
        let edge = self.px(SIDE_SCROLL_EDGE);
        if x < side.x || x >= side.right() {
            return 0.0;
        }
        // A carried pin is the window's own pointer, which goes on past the sidebar's ends.
        let inside = carrying_pin || (side.y..side.bottom()).contains(&y);
        let depth = if y < side.y + edge && inside && self.side_scroll > 0.0 {
            y - (side.y + edge)
        } else if y >= side.bottom() - edge && inside && self.side_scroll < self.max_scroll_in(Pane::Sidebar) {
            y - (side.bottom() - edge) + 1
        } else {
            return 0.0;
        };
        // Faster nearer the edge, and faster still past it.
        let step = (depth.abs().min(edge * 2) as f64 / edge as f64 * self.px(8.0) as f64).max(self.px(2.0) as f64);
        step.copysign(depth as f64)
    }

    /// Starts or stops the sidebar's scrolling under a drag, as the pointer comes near its edge
    /// or leaves it.
    fn arm_side_scroll(&mut self) {
        self.side_scroll_at = if self.side_drag_step() != 0.0 { self.side_scroll_at.or_else(|| Some(Instant::now() + BAND_SCROLL_EVERY)) } else { None };
    }

    fn side_scroll_on_timer(&mut self, now: Instant) {
        if !self.side_scroll_at.is_some_and(|t| t <= now) {
            return;
        }
        let step = self.side_drag_step();
        self.side_scroll_at = (step != 0.0).then(|| now + BAND_SCROLL_EVERY);
        self.side_scroll_to(self.side_scroll + step);
    }

    /// The place whose row is at `y`, if any: the heading isn't one.
    fn place_at(&self, y: i32) -> Option<usize> {
        let row = self.px(30.0);
        (0..self.places.len()).find(|&i| (self.place_top(i)..self.place_top(i) + row).contains(&y))
    }

    /// A drive's eject button, at the right of its row, for one that can be taken out.
    pub fn eject_rect(&self, i: usize) -> Option<Rect> {
        let p = self.places.get(i)?;
        if !p.volume.as_ref().is_some_and(|v| v.removable) {
            return None;
        }
        let l = self.layout();
        let w = self.px(26.0);
        Some(Rect::new(l.sidebar.right() - self.px(7.0) - self.side_bar_room() - w, self.place_top(i), w, self.px(30.0)))
    }

    /// The list's menu, with "Open with…" after Open, "Extract" and "Set as background" after
    /// Preview when they apply, and "Open in terminal" last.
    fn context_items(&self) -> Vec<MenuItem> {
        let mut items = self.view_menu_items();
        if self.open_with_files().is_some() {
            let at = items.iter().position(|m| m.action == Action::Open).map_or(0, |p| p + 1);
            items.insert(at, MenuItem { label: "Open with…", keys: "", action: Action::OpenWith, enabled: true });
        }
        if self.selected_archives().is_some() {
            let at = items.iter().position(|m| m.action == Action::Preview).map_or(items.len(), |p| p + 1);
            items.insert(at, MenuItem { label: "Extract", keys: "", action: Action::Extract, enabled: true });
        }
        if self.background_picture().is_some() {
            let at = items.iter().position(|m| m.action == Action::Preview).map_or(items.len(), |p| p + 1);
            items.insert(at, MenuItem { label: "Set as background", keys: "", action: Action::SetBackground, enabled: true });
        }
        if self.terminal_folder().is_some() {
            items.push(MenuItem { label: "Open in terminal", keys: "", action: Action::OpenTerminal, enabled: true });
        }
        items
    }

    /// The folder "Open in terminal" would open in: the one folder selected, or with nothing
    /// selected, the folder on screen. Only where `xdg-terminal-exec` is installed, and never
    /// in a chooser, the Trash or Recent, which aren't folders to work in.
    fn terminal_folder(&self) -> Option<PathBuf> {
        if self.terminal.is_none() || self.chooser.is_some() || self.viewer || self.is_recent_view() || self.in_trash() {
            return None;
        }
        let l = self.listing.as_ref()?;
        match self.selected_ids().as_slice() {
            [] => Some(self.dir.clone()),
            [one] if l.kind(*one).is_dirlike() => Some(l.path(*one)),
            _ => None,
        }
    }

    fn open_terminal(&mut self) {
        let (Some(dir), Some(terminal)) = (self.terminal_folder(), self.terminal.as_ref()) else { return };
        let name = dir.file_name().map_or_else(|| dir.to_string_lossy().into_owned(), |n| n.to_string_lossy().into_owned());
        let hub = self.hub.clone();
        terminal.open(&dir, move |why| hub.send(Msg::Notice(format!("Couldn't open a terminal in “{name}”: {why}"))));
    }

    /// The picture "Set as background" would use: the one file selected, if it's a picture, in
    /// a folder or Recent, and only where the desktop portal can set the background. Never in a
    /// chooser, or the Trash, whose files have to be restored first.
    fn background_picture(&self) -> Option<PathBuf> {
        if !self.can_set_background || self.session_bus.is_none() || self.chooser.is_some() || self.sel_count != 1 || self.in_trash() {
            return None;
        }
        let l = self.listing.as_ref()?;
        let id = *self.selected_ids().first()?;
        let picture = matches!(l.kind(id), Kind::File | Kind::Link) && crate::icons::icon_for_name(l.name(id)) == crate::icons::Icon::Image && !is_raw_or_icon(l.name(id));
        picture.then(|| l.path(id))
    }

    /// The files "Open with…" opens: the selection, when it's only files, and not too many.
    /// Archives count, so another app can open one rather than Gannet extracting it. Never in
    /// a chooser, which only picks, or the Trash, whose files have to be restored first.
    pub(crate) fn open_with_files(&self) -> Option<Vec<PathBuf>> {
        if self.chooser.is_some() || self.sel_count == 0 || self.sel_count > crate::openwith::MANY || self.in_trash() {
            return None;
        }
        let l = self.listing.as_ref().filter(|l| !l.is_trash())?;
        let ids = self.selected_ids();
        ids.iter().all(|&i| matches!(l.kind(i), Kind::File | Kind::Link)).then(|| ids.iter().map(|&i| l.path(i)).collect())
    }

    /// The archives selected, when that's all that's selected. Never in a chooser, where
    /// opening one picks it, or the Trash, whose files have to be restored first.
    fn selected_archives(&self) -> Option<Vec<PathBuf>> {
        if self.chooser.is_some() || self.sel_count == 0 || self.in_trash() {
            return None;
        }
        let l = self.listing.as_ref()?;
        let ids = self.selected_ids();
        ids.iter().all(|&i| is_archive(l, i)).then(|| ids.iter().map(|&i| l.path(i)).collect())
    }

    /// Unpacks each archive into a new folder beside it, which is selected when it's done.
    fn extract(&mut self, archives: Vec<PathBuf>) {
        // A folder made in the Trash's files would have no info to go with it.
        if self.in_trash() {
            self.notice("Restore it from the Trash first, then extract it".into());
            return;
        }
        self.start_job(Request::Extract { archives });
    }

    fn set_background(&mut self) {
        let (Some(path), Some(address)) = (self.background_picture(), self.session_bus.clone()) else { return };
        crate::wallpaper::set(self.hub.clone(), address, path);
    }

    fn view_menu_items(&self) -> Vec<MenuItem> {
        if self.chooser.is_some() {
            return self.chooser_menu_items();
        }
        let any = self.sel_count > 0;
        if self.is_recent_view() {
            return vec![
                MenuItem { label: "Open", keys: "Enter", action: Action::Open, enabled: any },
                MenuItem { label: "Preview", keys: "Space", action: Action::Preview, enabled: any },
                MenuItem { label: "Show in folder", keys: "Alt+Enter", action: Action::ShowInFolder, enabled: any },
                MenuItem { label: "Copy", keys: "Ctrl+C", action: Action::Copy, enabled: any },
                MenuItem { label: if self.show_hidden { "Hide hidden files" } else { "Show hidden files" }, keys: "Ctrl+H", action: Action::ToggleHidden, enabled: true },
                MenuItem { label: "Reload", keys: "F5", action: Action::Reload, enabled: true },
                MenuItem { label: "New window", keys: "Ctrl+N", action: Action::NewWindow, enabled: true },
            ];
        }
        if self.search_query().is_some() {
            // Matches come from many folders, so they're renamed one at a time.
            let one = self.sel_count == 1;
            return vec![
                MenuItem { label: "Open", keys: "Enter", action: Action::Open, enabled: any },
                MenuItem { label: "Preview", keys: "Space", action: Action::Preview, enabled: any },
                MenuItem { label: "Show in folder", keys: "Alt+Enter", action: Action::ShowInFolder, enabled: any },
                MenuItem { label: "Cut", keys: "Ctrl+X", action: Action::Cut, enabled: any },
                MenuItem { label: "Copy", keys: "Ctrl+C", action: Action::Copy, enabled: any },
                MenuItem { label: "Rename", keys: "F2", action: Action::Rename, enabled: one },
                MenuItem { label: "Move to Trash", keys: "Delete", action: Action::Trash, enabled: any },
                MenuItem { label: "Delete for good", keys: "Shift+Delete", action: Action::DeleteForGood, enabled: any },
                MenuItem { label: "Undo", keys: "Ctrl+Z", action: Action::Undo, enabled: !self.undo.is_empty() },
                MenuItem { label: "Redo", keys: "Ctrl+Shift+Z", action: Action::Redo, enabled: !self.redo.is_empty() },
                MenuItem { label: if self.show_hidden { "Hide hidden files" } else { "Show hidden files" }, keys: "Ctrl+H", action: Action::ToggleHidden, enabled: true },
                MenuItem { label: "Sort by name", keys: "Ctrl+1", action: Action::SortBy(SortKey::Name), enabled: true },
                MenuItem { label: "Sort by size", keys: "Ctrl+2", action: Action::SortBy(SortKey::Size), enabled: true },
                MenuItem { label: "Sort by modified", keys: "Ctrl+3", action: Action::SortBy(SortKey::Modified), enabled: true },
                MenuItem { label: "Search again", keys: "F5", action: Action::Reload, enabled: true },
                MenuItem { label: "New window", keys: "Ctrl+N", action: Action::NewWindow, enabled: true },
            ];
        }
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
            MenuItem { label: "Rename", keys: "F2", action: Action::Rename, enabled: any },
            MenuItem { label: "Move to Trash", keys: "Delete", action: Action::Trash, enabled: any },
            MenuItem { label: "Delete for good", keys: "Shift+Delete", action: Action::DeleteForGood, enabled: any },
            MenuItem { label: "New folder", keys: "Ctrl+Shift+N", action: Action::NewFolder, enabled: true },
            MenuItem { label: "Undo", keys: "Ctrl+Z", action: Action::Undo, enabled: !self.undo.is_empty() },
            MenuItem { label: "Redo", keys: "Ctrl+Shift+Z", action: Action::Redo, enabled: !self.redo.is_empty() },
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
        // Keys held, focus and drags all change whether a drag can go to the Trash, which shows
        // dimmed when it can't, so it's repainted whenever that changes.
        let trash_off = self.trash_unavailable();
        self.handle_input(i);
        if self.trash_unavailable() != trash_off {
            self.invalidate(self.layout().sidebar);
        }
        // Whatever just happened may have opened a menu or dialog, like a close request.
        self.refresh_file_drag();
        self.preview_follow();
        self.arm_side_scroll();
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
                // The release may never come, or come somewhere else.
                self.eject_pressed = None;
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
                if let Some(Dialog::RenameMany(_)) = &self.dialog {
                    let shown = self.rename_rows_shown();
                    let row = self.rename_layout(self.rename_count()).row_h as f64;
                    let by = (lines * 3.0 + pixels * self.scale / row).round() as isize;
                    if let Some(Dialog::RenameMany(d)) = self.dialog.as_mut() {
                        d.scroll = d.scroll.saturating_add_signed(by);
                        d.clamp_scroll(shown);
                    }
                    self.invalidate_all();
                } else if self.preview.is_some() {
                    self.preview_scroll(lines, pixels);
                } else if self.menu.is_none() && self.layout().sidebar.contains(self.pointer.0, self.pointer.1) && self.scrollbar_visible(Pane::Sidebar) {
                    // Over a sidebar that all fits, the wheel goes on scrolling the list.
                    let row = self.px(30.0) as f64;
                    self.side_scroll_to(self.side_scroll + lines * 3.0 * row + pixels * self.scale);
                } else if self.menu.is_none() {
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
            Input::DragEnter { x, y, files, picture, can_copy, can_move, marker } => self.drag_entered(x, y, files, picture, can_copy, can_move, marker),
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
        if let Some(d) = &self.pin_drag {
            let active = d.active;
            if sym == k::KEY_Escape {
                self.cancel_pin_drag();
            }
            if active || sym == k::KEY_Escape {
                return;
            }
        }
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
            self.dialog_key(sym, utf8, m);
        } else if self.menu.is_some() {
            self.menu_key(sym);
        } else if self.preview.is_some() {
            self.preview_key(sym, m);
        } else if !matches!(self.edit, Edit::None) {
            self.field_key(sym, utf8, m);
        } else if !(self.chooser.is_some() && self.chooser_key(sym, utf8, m)) {
            // A chooser's bar has first go at a key, and leaves the rest to the list.
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

    pub(crate) fn field_key(&mut self, sym: u32, utf8: &str, m: Mods) {
        let is_filter = matches!(self.edit, Edit::Filter(_));
        let selected = self.field_mut().map(|f| {
            let (a, b) = f.selection();
            f.text[a..b].to_string()
        });
        if m.ctrl && matches!(sym, k::KEY_c | k::KEY_x) && selected.as_ref().is_some_and(|t| !t.is_empty()) {
            self.copy_text(selected.unwrap_or_default());
            if sym == k::KEY_x
                && let Some(f) = self.field_mut()
            {
                f.backspace(false);
                self.field_edited();
            }
            return;
        }
        if m.ctrl && sym == k::KEY_v {
            self.cmds.push(Cmd::ReadClipboard(Paste::IntoField { edit: self.edit_gen }));
            return;
        }
        // In the box, Ctrl+F turns a filter into a search and back, keeping the text. / can't,
        // as it's typed.
        if is_filter && m.ctrl && !m.shift && sym == k::KEY_f {
            self.set_search_below(!self.search_below);
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
        self.field_edited();
    }

    /// Puts a field's text on the clipboard, as Ctrl+C in one does.
    fn copy_text(&mut self, t: String) {
        let t = t.into_bytes();
        self.cmds.push(Cmd::SetClipboard(vec![(clipboard::TEXT, t.clone()), (clipboard::TEXT_PLAIN, t.clone()), (clipboard::UTF8_STRING, t)]));
    }

    /// Follows a change to the text, caret or selection of the field with the keyboard,
    /// whatever made it: the filter narrows the list, and the rename dialog shows the new names.
    fn field_edited(&mut self) {
        self.apply_filter_field();
        if let Some(Dialog::RenameMany(d)) = self.dialog.as_mut() {
            d.update();
        }
        self.invalidate_all();
    }

    /// Ends an edit as a click elsewhere does. That commits a rename or new folder, as other
    /// file managers do, drops a typed path, and keeps what was typed for a filter.
    fn leave_field(&mut self) {
        match self.edit {
            Edit::None => {}
            Edit::Path(_) => self.field_cancel(),
            Edit::Rename { .. } | Edit::NewFolder(_) => self.field_commit(),
            Edit::Filter(_) => self.set_edit(Edit::None),
        }
    }

    fn field_cancel(&mut self) {
        let was_filter = matches!(self.edit, Edit::Filter(_));
        self.set_edit(Edit::None);
        if was_filter {
            self.clear_filter();
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
            // Alt+Enter goes to where a search's match or a recent file is.
            (k::KEY_Return | k::KEY_KP_Enter, _, true) if self.search_query().is_some() || self.is_recent_view() => self.action(Action::ShowInFolder),
            (k::KEY_Return | k::KEY_KP_Enter, _, _) => self.action(Action::Open),
            // Holding either of these would flicker, and neither means anything repeated.
            (k::KEY_space, _, false) if self.key_is_repeat => {}
            (k::KEY_space, false, false) => self.toggle_preview(),
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
                if self.previewer.is_some() {
                    self.close_preview();
                } else if !self.filter.is_empty() {
                    self.clear_filter();
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
            (k::KEY_z | k::KEY_Z, true, _) if m.shift => self.action(Action::Redo),
            (k::KEY_N, true, _) | (k::KEY_n, true, _) if m.shift => self.action(Action::NewFolder),
            (k::KEY_period, true, _) => self.stop_newest_job(),
            (k::KEY_F5, _, _) | (k::KEY_r, true, _) => self.action(Action::Reload),
            (k::KEY_F10, _, _) | (k::KEY_Menu, _, _) => self.keyboard_menu(),
            (k::KEY_h, true, _) => self.action(Action::ToggleHidden),
            (k::KEY_l, true, _) => self.start_path_edit(),
            (k::KEY_f, true, _) if !m.shift => self.start_search(),
            (k::KEY_a, true, _) => self.select_all(),
            (k::KEY_n, true, _) if !m.shift => self.action(Action::NewWindow),
            (k::KEY_q | k::KEY_w, true, _) => self.request_close(),
            (k::KEY_1, true, _) => self.action(Action::SortBy(SortKey::Name)),
            (k::KEY_2, true, _) => self.action(Action::SortBy(SortKey::Size)),
            (k::KEY_3, true, _) => self.action(Action::SortBy(SortKey::Modified)),
            (_, false, true) => {
                if let Some(d) = utf8.chars().next().and_then(|c| c.to_digit(10)).filter(|&d| d > 0) {
                    self.reveal_place(d as usize - 1);
                    self.open_place(d as usize - 1);
                }
            }
            (_, false, false) if !m.logo && utf8 == "/" => self.start_filter_here(),
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
            let renaming = matches!(d, Dialog::RenameMany(_));
            self.dialog = Some(d);
            if check {
                return Hit::DialogCheck;
            }
            if renaming {
                let l = self.rename_layout(self.rename_count());
                if l.find.contains(x, y) {
                    return Hit::DialogField(Focus::Find);
                }
                if l.replace.contains(x, y) {
                    return Hit::DialogField(Focus::Replace);
                }
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
        if let Some(h) = self.chooser_hit(x, y) {
            return h;
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
            if self.on_side_scrollbar(x) {
                return Hit::Scrollbar(Pane::Sidebar);
            }
            if let Some(i) = self.place_at(y) {
                if self.eject_rect(i).is_some_and(|r| r.contains(x, y)) {
                    return Hit::Eject(i);
                }
                return Hit::Place(i);
            }
            return Hit::None;
        }
        if l.header.contains(x, y) {
            // Recent's order is fixed, so its headings aren't buttons.
            if self.is_recent_view() {
                return Hit::None;
            }
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
            if self.scrollbar_visible(Pane::List) && x >= l.list.right() - self.px(12.0) {
                return Hit::Scrollbar(Pane::List);
            }
            let row = ((y - l.list.y) as f64 + self.scroll) as i64 / l.row_h.max(1) as i64;
            if (row as usize) < self.rows.len() {
                return Hit::Row(row as usize);
            }
            return Hit::ListEmpty;
        }
        Hit::None
    }

    pub fn scrollbar_visible(&self, pane: Pane) -> bool {
        self.max_scroll_in(pane) > 0.0
    }

    /// The thumb's top and height, in the pane's coordinates.
    pub fn scrollbar_thumb(&self, pane: Pane) -> (i32, i32) {
        let view = self.pane_rect(pane).h;
        let content = self.pane_content(pane).max(1.0);
        let h = ((view as f64 / content * view as f64) as i32).max(self.px(24.0)).min(view);
        let travel = (view - h) as f64;
        let max = self.max_scroll_in(pane);
        let y = if max > 0.0 { (self.pane_scroll(pane) / max * travel) as i32 } else { 0 };
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
        let parts = self.crumb_parts();
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

    /// Every part of the path, from the top, as its label and folder, whether or not it fits.
    pub fn crumb_parts(&self) -> Vec<(String, PathBuf)> {
        let mut parts: Vec<(String, PathBuf)> = Vec::new();
        let mut acc = PathBuf::new();
        // Recent has no folders above it.
        let recent = self.is_recent_view();
        if recent {
            parts.push(("Recent".into(), self.dir.clone()));
        }
        // Inside any Trash, its real path is noise: it starts at "Trash".
        let mut components = self.dir.components();
        let files = self.trash_files_root(&self.dir);
        if let Some((f, rest)) = files.as_ref().and_then(|f| Some((f, self.dir.strip_prefix(f).ok()?))) {
            // "Trash" opens the Trash view, and what's below it is where it really is.
            parts.push(("Trash".into(), self.trash_dir.clone()));
            acc = f.clone();
            components = rest.components();
        }
        for c in components.filter(|_| !recent) {
            acc.push(c);
            let label = match c {
                std::path::Component::RootDir => "/".to_string(),
                other => other.as_os_str().to_string_lossy().into_owned(),
            };
            parts.push((label, acc.clone()));
        }
        parts
    }

    /// What a folder is called in the path bar and the window title.
    pub fn crumb_label(&self, path: &Path) -> String {
        if path == self.trash_dir {
            return "Trash".into();
        }
        if path == self.recent_file {
            return "Recent".into();
        }
        title_for(path)
    }

    /// Where a search's match is, starting from the folder searched: "Documents/photos" for one
    /// in ~/Documents/photos when ~/Documents was searched, and "Documents" for one in it.
    pub fn search_location(&self, folder: &Path) -> String {
        search_location(folder, &self.dir, &self.crumb_label(&self.dir))
    }

    /// What the list says in place of rows when it has none: why the folder can't be shown,
    /// that it's being searched, or that nothing's there. The second is whether it's an error.
    pub fn list_message(&self) -> Option<(String, bool)> {
        let searching = self.search_query().is_some();
        let Some(listing) = self.listing.as_deref() else {
            if let Some(e) = &self.load_error {
                return Some((format!("Can't open this folder: {e}"), true));
            }
            return (self.loading && self.hold_paint.is_none() && searching).then(|| ("Searching…".to_string(), false));
        };
        if !self.rows.is_empty() {
            return None;
        }
        if searching && self.loading {
            return Some(("Searching…".to_string(), false));
        }
        if self.loading {
            return None;
        }
        let msg = if searching {
            format!("Nothing found in {} or the folders in it", self.crumb_label(&self.dir))
        } else if !self.filter.is_empty() && self.can_search() {
            "Nothing here matches. Ctrl+F searches the folders inside too".to_string()
        } else if !self.filter.is_empty() {
            "Nothing matches the filter".to_string()
        } else if listing.is_trash() {
            "The Trash is empty".to_string()
        } else if listing.is_recent() {
            "No recent files".to_string()
        } else {
            "This folder is empty".to_string()
        };
        Some((msg, false))
    }

    /// Goes up the path to one of its folders, with the cursor on the folder come from.
    fn open_crumb(&mut self, path: PathBuf) {
        if path == self.dir {
            return;
        }
        let name = if path == self.trash_dir && let Some(files) = self.trash_files_root(&self.dir) {
            // The Trash view finds rows by their real path, not their name.
            self.dir.strip_prefix(&files).ok().and_then(|r| r.components().next()).map(|c| files.join(c).into_os_string().into_vec())
        } else {
            self.dir.strip_prefix(&path).ok().and_then(|r| r.components().next()).map(|c| std::os::unix::ffi::OsStrExt::as_bytes(c.as_os_str()).to_vec())
        };
        self.navigate(path, name);
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
        if self.chooser.is_some() {
            self.invalidate(l.status);
        }
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
        if self.preview_drag(px, py) || self.pin_drag_moved(px, py) {
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
        if let Some((pane, grab)) = self.drag_scroll {
            let area = self.pane_rect(pane);
            let (_, h) = self.scrollbar_thumb(pane);
            let travel = (area.h - h).max(1) as f64;
            let top = (py - area.y - grab) as f64;
            self.scroll_pane_to(pane, top / travel * self.max_scroll_in(pane));
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
                self.pin_released();
                self.finish_file_drag();
                if let Some(block) = self.eject_pressed.take()
                    && let Hit::Eject(i) = self.hit(self.pointer.0, self.pointer.1)
                    && let Some(v) = self.places.get(i).and_then(|p| p.volume.clone()).filter(|v| v.block == block)
                {
                    self.drive_act(v, Doing::Eject);
                }
            }
            return;
        }
        // A release that went missing, as when the compositor took the pointer away, ejects nothing.
        self.eject_pressed = None;
        // The release of a drag can go missing if the compositor takes the pointer away.
        self.cancel_file_drag();
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
                    Hit::DialogField(f) => {
                        if let Some(Dialog::RenameMany(d)) = self.dialog.as_mut() {
                            d.focus = f;
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
        if !matches!(hit, Hit::PathBar | Hit::FilterBox) {
            self.leave_field();
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
            } else if let Hit::Place(i) | Hit::Eject(i) = self.hit(x, y)
                && let Some(v) = self.places.get(i).and_then(|p| p.volume.clone())
            {
                self.open_drive_menu(v, x, y);
            } else if let Hit::Place(i) = self.hit(x, y)
                && let Some(p) = self.places.get(i).filter(|p| p.kind == PlaceKind::Pinned)
            {
                self.open_pin_menu(p.path.clone(), x, y);
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
                if let Some((_, _, path)) = self.crumbs().into_iter().nth(i) {
                    self.open_crumb(path);
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
            // A pin opens on the release, as the press can turn into carrying it elsewhere.
            Hit::Place(i) if self.places.get(i).is_some_and(|p| p.kind == PlaceKind::Pinned) => self.press_pin(i, x, y),
            Hit::Place(i) => self.open_place(i),
            Hit::Eject(i) => self.eject_pressed = self.places.get(i).and_then(|p| p.volume.as_ref()).map(|v| v.block.clone()),
            Hit::Header(key) => self.action(Action::SortBy(key)),
            Hit::Scrollbar(pane) => {
                let area = self.pane_rect(pane);
                let (top, h) = self.scrollbar_thumb(pane);
                let inside = y - area.y - top;
                if inside >= 0 && inside < h {
                    self.drag_scroll = Some((pane, inside));
                } else {
                    // A click on the track jumps there, and the thumb follows the pointer.
                    self.drag_scroll = Some((pane, h / 2));
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
            Hit::DialogButton(_) | Hit::DialogCheck | Hit::DialogField(_) | Hit::PreviewBody | Hit::PinGap(_) => {}
            Hit::ChooserName | Hit::ChooserFilter | Hit::ChooserAccept | Hit::ChooserCancel => self.chooser_click(hit, x),
            Hit::Row(row) => {
                self.chooser_clicked_list(Some(row));
                // Read first: a press that opens the row changes what's listed.
                let name = self.listing.as_ref().map_or_else(Vec::new, |l| l.key(self.rows[row]).into_owned());
                let band = (!self.on_row_name(row, x)).then(|| (self.save_selection(), self.list_y(y)));
                if let Some(narrow) = self.click_row(row) {
                    self.file_drag = Some(FileDrag::Pressed { at: (x, y), row, dir: self.dir.clone(), name, narrow, band });
                }
            }
            Hit::ListEmpty => {
                self.chooser_clicked_list(None);
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
        let Some(f) = self.field_mut().map(|f| f.clone()) else { return };
        // The same scroll the painter applies when the text is longer than the box.
        let x0 = self.field_text_x(r, &f);
        let at = self.text.offset_at(&f.text, x as f32 - x0);
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
        let folders = !self.in_trash() && self.listing.as_ref().is_some_and(|l| self.selected_ids().iter().all(|&i| l.kind(i).is_dirlike()));
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
        self.file_drag = Some(FileDrag::Active(ActiveDrag { paths, dir: from, probe, id, source: None, devices: HashMap::new(), target: None, label: Rect::default(), ghost, ghost_at: Rect::default(), wayland: false, handed_over: None, inside: false, moved_inside: false, accepting: false, folders }));
    }

    /// The folder a drop here would land in. Dropping on what's being dragged, on the folder
    /// it's already in, or inside itself isn't a target.
    fn drop_target_at(&mut self, d: &ActiveDrag, x: i32, y: i32) -> Option<DropTarget> {
        // A chooser doesn't move files about, by drag any more than by cut and paste.
        if self.chooser.is_some() {
            return None;
        }
        // Between places, folders are pinned. Ones pinned already stay where they are, so there's
        // nothing to do with only those.
        // Folders that are in the sidebar already, as a pin or a standard place, fall through to
        // whatever's under them.
        if d.folders
            && !d.paths.iter().all(|p| self.in_sidebar(p))
            && let Some(gap) = self.pin_gap_at(x, y)
        {
            return Some(DropTarget { hit: Hit::PinGap(gap), dest: PathBuf::new(), trash: false, pin: Some(self.spot(gap)) });
        }
        let hit = self.hit(x, y);
        match hit {
            Hit::Row(row) => {
                let l = self.listing.as_ref()?;
                let id = *self.rows.get(row)?;
                if self.list_takes_no_drops() || !l.kind(id).is_dirlike() || self.is_selected(id) {
                    return None;
                }
                Some(DropTarget { hit, dest: l.path(id), trash: false, pin: None })
            }
            Hit::Place(i) => {
                let place = self.places.get(i)?;
                // A drive that isn't mounted has no folder to drop into yet, and Recent isn't a folder.
                if place.path.as_os_str().is_empty() || place.kind == PlaceKind::Recent {
                    return None;
                }
                let trash = place.kind == PlaceKind::Trash;
                // Ctrl asks for a copy, and trashing the originals would be the opposite.
                if place.path == self.dir || (trash && (self.copies_only() || self.mods_now.ctrl)) {
                    return None;
                }
                let inside_itself = place.path.strip_prefix(&self.dir).ok().and_then(|rest| rest.components().next()).is_some_and(|first| d.paths.iter().any(|p| p.file_name() == Some(first.as_os_str())));
                if inside_itself {
                    return None;
                }
                Some(DropTarget { hit, dest: place.path.clone(), trash, pin: None })
            }
            _ => None,
        }
    }

    pub(crate) fn drop_action(&self, d: &ActiveDrag, t: &DropTarget) -> DropAction {
        use gannet_core::ops::{DropVerb, drop_verb};
        if t.trash {
            return DropAction::Trash;
        }
        if t.pin.is_some() {
            return DropAction::Pin;
        }
        // The Trash's items can only leave as copies, as with cut and paste, and so can Recent's.
        if self.copies_only() {
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
        if t.pin.is_some() {
            return if d.paths.len() == 1 { format!("Pin {what}") } else { format!("Pin {} folders", d.paths.len()) };
        }
        let folder = t.dest.file_name().map_or_else(|| t.dest.to_string_lossy().into_owned(), |n| n.to_string_lossy().into_owned());
        match self.drop_action(d, t) {
            DropAction::Move => format!("Move {what} to {folder}"),
            DropAction::Copy => format!("Copy {what} to {folder}"),
            DropAction::Link => format!("Link {what} in {folder}"),
            DropAction::Trash => format!("Move {what} to Trash"),
            DropAction::Pin => format!("Pin {what}"),
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
            && t.pin.is_none()
            && !d.devices.contains_key(&t.dest)
        {
            d.devices.insert(t.dest.clone(), None);
            let _ = d.probe.send(t.dest.clone());
        }
        let cursor = d.target.as_ref().map_or(Cursor::NoDrop, |t| match self.drop_action(&d, t) {
            DropAction::Move | DropAction::Trash => Cursor::Move,
            DropAction::Copy => Cursor::Copy,
            DropAction::Link | DropAction::Pin => Cursor::Alias,
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
        self.cancel_pin_drag();
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

    /// Puts a pin being carried back where it was, and forgets a press on one.
    fn cancel_pin_drag(&mut self) {
        if self.pin_drag.take().is_some() {
            self.end_pin_drag();
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
        let move_only = self.mods_now.shift && !self.mods_now.ctrl && !self.copies_only();
        let marker = source.filter(|_| !self.copies_only()).map(|(device, can_delete)| clipboard::DragMarker { device, can_delete, ctrl: self.mods_now.ctrl, shift: self.mods_now.shift });
        let icon = self.render_drag_icon(&ghost);
        self.cmds.push(Cmd::StartDrag { formats, marker, move_only, icon });
    }

    /// The position that comes with a drag's arrival can't be believed: Hyprland always says
    /// the middle of the window. So nothing under it lights up or gets a label, and the real
    /// position comes with the first motion, right behind it.
    #[allow(clippy::too_many_arguments)]
    fn drag_entered(&mut self, x: f64, y: f64, files: bool, picture: bool, can_copy: bool, can_move: bool, marker: Option<clipboard::DragMarker>) {
        // A drag arriving means the button came up without this window hearing about it.
        if matches!(self.file_drag, Some(FileDrag::Band(_) | FileDrag::Pressed { .. })) {
            self.cancel_file_drag();
        }
        self.cancel_pin_drag();
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
        self.incoming = Some(IncomingDrag { files, picture, can_copy, can_move, marker, probe, id, devices: HashMap::new(), moved: false, target: None, label: Rect::default(), accepting: false });
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

    /// Whether a drag from elsewhere is over the window that can't go to the Trash, which then
    /// shows dimmed.
    pub(crate) fn trash_unavailable(&self) -> bool {
        self.incoming.as_ref().is_some_and(|i| i.files && !self.incoming_can_trash(i))
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
        if !i.files || self.chooser.is_some() {
            return None;
        }
        if !i.moved {
            let refused = self.list_takes_no_drops() || self.dialog.is_some() || self.menu.is_some() || self.preview.is_some();
            return (!refused).then(|| DropTarget { hit: Hit::None, dest: self.dir.clone(), trash: false, pin: None });
        }
        let (x, y) = self.pointer;
        self.incoming_target_at(self.incoming_can_trash(i), x, y)
    }

    /// Where a drop from elsewhere would land. A folder or place under the pointer, and
    /// otherwise the folder showing.
    fn incoming_target_at(&mut self, can_trash: bool, x: i32, y: i32) -> Option<DropTarget> {
        // Between places, it's pinned. Whether it's folders is only known once it's dropped.
        if let Some(gap) = self.pin_gap_at(x, y) {
            return Some(DropTarget { hit: Hit::PinGap(gap), dest: PathBuf::new(), trash: false, pin: Some(self.spot(gap)) });
        }
        let hit = self.hit(x, y);
        match hit {
            Hit::Row(_) | Hit::ListEmpty if self.list_takes_no_drops() => None,
            Hit::Row(row) => {
                let l = self.listing.as_ref()?;
                let id = *self.rows.get(row)?;
                if l.kind(id).is_dirlike() {
                    return Some(DropTarget { hit, dest: l.path(id), trash: false, pin: None });
                }
                Some(DropTarget { hit: Hit::ListEmpty, dest: self.dir.clone(), trash: false, pin: None })
            }
            Hit::ListEmpty => Some(DropTarget { hit, dest: self.dir.clone(), trash: false, pin: None }),
            Hit::Place(i) => {
                let place = self.places.get(i)?;
                // A drive that isn't mounted has no folder to drop into yet, and Recent isn't a folder.
                if place.path.as_os_str().is_empty() || place.kind == PlaceKind::Recent {
                    return None;
                }
                let trash = place.kind == PlaceKind::Trash;
                // Trashing takes the files away, so the other program has to allow that.
                if trash && !can_trash {
                    return None;
                }
                Some(DropTarget { hit, dest: place.path.clone(), trash, pin: None })
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
    pub(crate) fn incoming_can_trash(&self, i: &IncomingDrag) -> bool {
        if i.picture {
            return false;
        }
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
        if t.pin.is_some() {
            return DropAction::Pin;
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
            DropAction::Pin => "Pin to the sidebar".to_string(),
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
            && t.pin.is_none()
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
                self.cmds.push(Cmd::ReadDrop { dest: t.dest, trash: t.trash, decided, pin: t.pin });
            }
            None => self.cmds.push(Cmd::AbandonDrop),
        }
    }

    /// Acts on a dropped file list. This window does the work, so progress, conflicts and
    /// undo are the same as anywhere else. Another Gannet window said what it knows, so the
    /// rule inside a window applies. A drop from another program copies unless that program
    /// only offers a move, because its keys and disks can't be seen from here.
    fn on_dropped(&mut self, d: Dropped) {
        if let Some(at) = d.pin {
            // Pinning leaves everything where it was. Hearing the drop finished, a program that
            // only offered a move may delete its side, so only one that allows a copy, or another
            // Gannet window, hears that. It's answered first: asking whether each item is a folder
            // can hang on a disk that's stopped answering, and some compositors keep the pointer
            // from every window until a drop is answered.
            self.cmds.push(if d.can_copy || d.decided.is_some() { Cmd::FinishDrop } else { Cmd::AbandonDrop });
            if d.paths.is_empty() {
                self.notice("Couldn't get any folders from that drag".into());
                return;
            }
            let hub = self.hub.clone();
            let started = std::thread::Builder::new().name("gannet-pin".into()).spawn(move || {
                let total = d.paths.len();
                let folders: Vec<PathBuf> = d.paths.into_iter().filter(|p| p.is_dir()).collect();
                hub.send(Msg::PinDropped { not_folders: total - folders.len(), folders, at });
            });
            if let Err(e) = started {
                self.notice(format!("Couldn't pin those folders: {e}"));
            }
            return;
        }
        match d.saved {
            Some(Ok(path)) => {
                let name = path.file_name().map(|n| std::os::unix::ffi::OsStrExt::as_bytes(n).to_vec()).unwrap_or_default();
                if path.parent() == Some(self.dir.as_path()) {
                    self.pending_select = vec![name];
                    self.reload();
                }
                // Saved in a folder it was dropped on, it's there to see, as with a copy.
                // Hearing the drop finished, a program that only offered a move may delete its
                // side, so that's said only to one that allows a copy, which is what was made.
                self.cmds.push(if d.can_copy { Cmd::FinishDrop } else { Cmd::AbandonDrop });
                return;
            }
            Some(Err(why)) => {
                self.notice(format!("Couldn't save the picture: {why}"));
                self.cmds.push(Cmd::AbandonDrop);
                return;
            }
            None => {}
        }
        if d.paths.is_empty() && d.link {
            self.notice("That drag held a link, not a file".into());
            self.cmds.push(Cmd::AbandonDrop);
            return;
        }
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
                if let Some(at) = t.pin {
                    return self.pin_folders(d.paths, at);
                }
                let action = self.drop_action(&d, &t);
                let (sources, dest) = (d.paths, t.dest);
                self.start_job(match action {
                    DropAction::Move => Request::Move { sources, dest },
                    DropAction::Copy => Request::Copy { sources, dest },
                    DropAction::Link => Request::Link { sources, dest },
                    DropAction::Trash => Request::Trash { paths: sources },
                    // Handled above, as it isn't a job.
                    DropAction::Pin => return,
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
        Hit::Header(_) | Hit::Crumb(_) | Hit::Back | Hit::Forward | Hit::Up | Hit::EmptyTrash | Hit::Place(_) | Hit::Eject(_) | Hit::MenuItem(_) | Hit::NoticeClose(_) | Hit::JobCancel(_) | Hit::DialogButton(_) | Hit::DialogCheck | Hit::ChooserFilter | Hit::ChooserAccept | Hit::ChooserCancel => Cursor::Pointer,
        Hit::PathBar | Hit::FilterBox | Hit::ChooserName | Hit::DialogField(_) => Cursor::Text,
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

/// Where a search found something, from the folder searched, which is called `top`.
pub fn search_location(folder: &Path, dir: &Path, top: &str) -> String {
    match folder.strip_prefix(dir) {
        Ok(rest) if rest.as_os_str().is_empty() => top.to_string(),
        Ok(rest) => format!("{}/{}", top.trim_end_matches('/'), rest.display()),
        Err(_) => folder.display().to_string(),
    }
}

/// The folder a row of Recent or the Trash is in, with the home folder as `~`.
pub fn home_location(folder: &Path, home: Option<&Path>) -> String {
    match home.and_then(|h| folder.strip_prefix(h).ok()) {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Some(rest) => format!("~/{}", rest.display()),
        None => folder.display().to_string(),
    }
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

/// Whether row `i` is a file Gannet unpacks when it's opened.
fn is_archive(l: &Listing, i: u32) -> bool {
    matches!(l.kind(i), Kind::File | Kind::Link) && gannet_core::archive::is_archive(l.name(i))
}

/// Camera raw files and icons show as pictures, but desktops don't take them as a background.
fn is_raw_or_icon(name: &[u8]) -> bool {
    let ext = name.rsplit(|&b| b == b'.').next().unwrap_or_default().to_ascii_lowercase();
    matches!(&ext[..], b"raw" | b"cr2" | b"nef" | b"ico")
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

/// Adds to an undo or redo list, dropping the oldest past `UNDO_DEPTH`.
fn push_capped(list: &mut Vec<Undo>, u: Undo) {
    list.push(u);
    if list.len() > UNDO_DEPTH {
        list.remove(0);
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
            self.app.go(Stop { dir, cursor_name: None, search: None });
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

    // ---- Searching below ----

    const CTRL_SHIFT: Mods = Mods { ctrl: true, shift: true, alt: false, logo: false };

    impl Rig {
        /// Types into whatever field has the keyboard.
        fn type_text(&mut self, text: &str) {
            for c in text.chars() {
                self.app.key(0, &c.to_string(), Mods::default());
            }
        }

        /// Starts a search for `q` and waits for all of it.
        fn search(&mut self, q: &str) {
            self.ctrl(k::KEY_f);
            self.type_text(q);
            self.settle_search(q);
        }

        fn settle_search(&mut self, q: &str) {
            self.pump_until("search done", |a| !a.loading && a.listing.as_ref().is_some_and(|l| l.complete && l.search_query() == Some(q)));
        }

        /// Each row's name and where it is, as the Location column says.
        fn found(&self) -> Vec<(String, String)> {
            let l = self.app.listing.clone().unwrap();
            self.app.rows.iter().map(|&i| (String::from_utf8_lossy(l.name(i)).into_owned(), l.location(i).map(|f| self.app.search_location(f)).unwrap_or_default())).collect()
        }

        fn row_names(&self) -> Vec<String> {
            let l = self.app.listing.clone().unwrap();
            self.app.rows.iter().map(|&i| String::from_utf8_lossy(l.name(i)).into_owned()).collect()
        }
    }

    fn found(v: &[(&str, &str)]) -> Vec<(String, String)> {
        v.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect()
    }

    fn search_rig(tag: &str) -> Rig {
        let mut r = Rig::new(tag);
        for p in ["notes.txt", "other.txt", "a/b/notes-deep.md", "a/c/plain", ".hid/notes-hidden", "a/notes-dir/inside"] {
            let p = r.work().join(p);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, "x").unwrap();
        }
        let w = r.work();
        r.open(w);
        r
    }

    #[test]
    fn ctrl_f_searches_every_folder_below_and_escape_comes_back() {
        let mut r = search_rig("search-basic");
        r.search("notes");
        assert!(matches!(r.app.edit, Edit::Filter(_)) && r.app.search_below);
        assert_eq!(r.found(), found(&[("notes-dir", "work/a"), ("notes-deep.md", "work/a/b"), ("notes.txt", "work")]), "folders first, each with where it is");
        assert!(r.app.status_text().starts_with("3 items found in work"), "{}", r.app.status_text());
        // Escape ends the search, and the folder is back as it was.
        r.key(k::KEY_Escape, "");
        assert!(!r.app.search_below && r.app.filter.is_empty());
        r.pump_until("the folder again", |a| !a.loading);
        assert_eq!(r.row_names(), ["a", "notes.txt", "other.txt"]);
        assert!(r.app.listing.as_ref().unwrap().search().is_none());
    }

    #[test]
    fn a_search_is_never_kept_as_what_the_folder_holds() {
        let mut r = search_rig("search-cache");
        r.search("notes");
        r.key(k::KEY_Escape, "");
        // The folder shows at once from what was read before the search.
        assert_eq!(r.row_names(), ["a", "notes.txt", "other.txt"]);
        assert!(r.app.cache.iter().all(|c| c.listing.search().is_none()));
    }

    #[test]
    fn typing_more_narrows_what_shows_at_once_and_searches_again() {
        let mut r = search_rig("search-narrow");
        r.search("notes");
        r.type_text("-d");
        // Before the new search answers, what's showing already fits it.
        assert_eq!(r.row_names(), ["notes-dir", "notes-deep.md"]);
        r.settle_search("notes-d");
        assert_eq!(r.row_names(), ["notes-dir", "notes-deep.md"]);
        // Fewer letters find more again.
        r.app.key(k::KEY_BackSpace, "", Mods::default());
        r.app.key(k::KEY_BackSpace, "", Mods::default());
        r.settle_search("notes");
        assert_eq!(r.row_names().len(), 3);
    }

    #[test]
    fn ctrl_f_in_the_box_turns_a_search_into_a_filter_and_back() {
        let mut r = search_rig("search-toggle");
        r.search("notes");
        r.ctrl(k::KEY_f);
        assert!(!r.app.search_below && r.app.filter == "notes");
        r.pump_until("the folder, filtered", |a| !a.loading && a.listing.as_ref().is_some_and(|l| l.search().is_none()));
        assert_eq!(r.row_names(), ["notes.txt"]);
        r.ctrl(k::KEY_f);
        r.settle_search("notes");
        assert_eq!(r.row_names().len(), 3);
        // Ctrl+Shift+F is nothing, in the box or out of it.
        r.app.key(k::KEY_F, "", CTRL_SHIFT);
        assert!(r.app.search_below && r.app.filter == "notes");
        r.key(k::KEY_Escape, "");
        r.app.key(k::KEY_F, "", CTRL_SHIFT);
        assert!(matches!(r.app.edit, Edit::None));
    }

    #[test]
    fn opening_a_folder_found_and_going_back_returns_to_the_search_on_it() {
        let mut r = search_rig("search-back");
        r.search("notes");
        r.key(k::KEY_Down, "");
        r.select("notes-dir");
        r.key(k::KEY_Return, "");
        r.pump_until("in the folder found", |a| !a.loading && a.dir.ends_with("a/notes-dir"));
        assert!(!r.app.search_below && r.app.filter.is_empty());
        assert_eq!(r.row_names(), ["inside"]);
        r.app.go_back();
        r.settle_search("notes");
        assert!(r.app.search_below && r.app.filter == "notes");
        assert_eq!(r.selected_names(), ["notes-dir"], "on the folder it left from");
        // And forward again.
        r.app.go_forward();
        r.pump_until("in it again", |a| !a.loading && a.dir.ends_with("a/notes-dir"));
    }

    #[test]
    fn alt_enter_shows_a_match_in_its_folder() {
        let mut r = search_rig("search-reveal");
        r.search("notes");
        r.key(k::KEY_Down, "");
        r.select("notes-deep.md");
        r.app.key(k::KEY_Return, "", Mods { alt: true, ..Mods::default() });
        r.pump_until("its folder", |a| !a.loading && a.dir.ends_with("a/b"));
        assert_eq!(r.selected_names(), ["notes-deep.md"]);
        r.app.go_back();
        r.settle_search("notes");
        assert_eq!(r.selected_names(), ["notes-deep.md"], "back on it in the search");
    }

    #[test]
    fn hidden_folders_are_searched_only_while_hidden_files_show() {
        let mut r = search_rig("search-hidden");
        r.search("notes");
        r.key(k::KEY_Down, "");
        assert!(!r.row_names().contains(&"notes-hidden".to_string()));
        r.ctrl(k::KEY_h);
        r.pump_until("searched again", |a| !a.loading && a.rows.len() == 4);
        assert!(r.found().contains(&("notes-hidden".to_string(), "work/.hid".to_string())));
        r.ctrl(k::KEY_h);
        r.pump_until("and again", |a| !a.loading && a.rows.len() == 3);
    }

    #[test]
    fn a_match_moved_to_the_trash_leaves_the_results() {
        let mut r = search_rig("search-trash");
        r.search("notes");
        r.key(k::KEY_Down, "");
        r.select("notes-deep.md");
        r.key(k::KEY_Delete, "");
        r.idle();
        r.pump_until("searched again", |a| !a.loading && a.rows.len() == 2);
        assert!(!r.work().join("a/b/notes-deep.md").exists());
        assert!(r.app.search_below && r.app.filter == "notes", "still searching");
        // Undo puts it back, and it's found again.
        r.ctrl(k::KEY_z);
        r.idle();
        r.pump_until("found again", |a| !a.loading && a.rows.len() == 3);
    }

    #[test]
    fn renaming_a_match_works_where_it_is() {
        let mut r = search_rig("search-rename");
        r.search("notes");
        r.key(k::KEY_Down, "");
        r.select("notes-deep.md");
        r.key(k::KEY_F2, "");
        assert!(matches!(&r.app.edit, Edit::Rename { path, .. } if path.ends_with("a/b/notes-deep.md")));
        r.ctrl(k::KEY_a);
        r.type_text("notes-renamed.md");
        r.key(k::KEY_Return, "");
        r.idle();
        r.pump_until("searched again", |a| !a.loading && a.listing.as_ref().is_some_and(|l| (0..l.len() as u32).any(|i| l.name(i) == b"notes-renamed.md")));
        assert!(r.work().join("a/b/notes-renamed.md").exists());
        assert_eq!(r.selected_names(), ["notes-renamed.md"], "the renamed match stays selected");
    }

    #[test]
    fn saving_over_a_match_in_a_save_dialog_saves_where_the_match_is() {
        let mut r = search_rig("search-save");
        let spec = crate::chooser::Spec { mode: crate::chooser::Mode::Save, ..crate::chooser::Spec::default() };
        r.app.chooser = Some(crate::chooser::Chooser::new(spec, &[], None));
        r.search("notes");
        let l = r.app.listing.clone().unwrap();
        let deep = (0..l.len() as u32).find(|&i| l.name(i) == b"notes-deep.md").unwrap();
        r.app.chooser_open(vec![deep]);
        let deep_path = r.work().join("a/b/notes-deep.md");
        assert!(matches!(&r.app.dialog, Some(Dialog::ConfirmReplace { existing, .. }) if *existing == [deep_path.clone()]), "asks about the match itself");
        assert!(!r.work().join("notes-deep.md").exists());
        // Leaving the search leaves just the name, to save in the folder showing.
        r.app.dialog = None;
        r.key(k::KEY_Escape, "");
        assert_eq!(r.app.chooser.as_ref().unwrap().name.text, "notes-deep.md");
        // A full path typed in stays as it is when the folder changes.
        let typed = r.work().join("a/b/typed.md").to_string_lossy().into_owned();
        r.app.chooser.as_mut().unwrap().name = Field::new(&typed);
        r.app.go_up();
        r.pump_until("the folder above", |a| !a.loading);
        assert_eq!(r.app.chooser.as_ref().unwrap().name.text, typed);
    }

    #[test]
    fn slash_filters_only_here_and_ctrl_f_searches_whatever_came_before() {
        let mut r = search_rig("search-then-filter");
        r.ctrl(k::KEY_f);
        // Enter on the empty search box, then a plain filter.
        r.key(k::KEY_Return, "");
        r.key(0, "/");
        r.type_text("notes");
        assert!(!r.app.search_below && r.app.search_query().is_none());
        r.pump_until("filtered", |a| !a.loading);
        assert_eq!(r.row_names(), ["notes.txt"]);
        // Out of the box, Ctrl+F searches for what the filter has.
        r.key(k::KEY_Return, "");
        r.ctrl(k::KEY_f);
        assert!(matches!(r.app.edit, Edit::Filter(_)));
        r.settle_search("notes");
        assert_eq!(r.row_names().len(), 3);
        // And / after a search filters the folder for it.
        r.key(k::KEY_Return, "");
        r.key(0, "/");
        assert!(matches!(r.app.edit, Edit::Filter(_)) && !r.app.search_below && r.app.filter == "notes");
        r.key(k::KEY_Escape, "");
        r.ctrl(k::KEY_f);
        assert!(matches!(r.app.edit, Edit::Filter(_)) && r.app.search_below);
    }

    #[test]
    fn the_preview_window_stepping_moves_the_cursor_through_matches() {
        let mut r = search_rig("search-preview");
        r.search("notes");
        r.key(k::KEY_Down, "");
        r.select("notes-dir");
        r.app.previewer = Some(r.work().join("a/notes-dir"));
        let generation = r.app.previewer_gen;
        r.app.on_preview_stepped(generation, r.work().join("a/b/notes-deep.md"));
        assert_eq!(r.selected_names(), ["notes-deep.md"]);
    }

    #[test]
    fn a_search_isnt_a_folder_to_paste_or_make_a_folder_in_or_drop_on() {
        let mut r = search_rig("search-readonly");
        r.search("notes");
        r.app.action(Action::NewFolder);
        r.app.action(Action::Paste);
        assert_eq!(r.app.notices, ["Clear the search to make a folder here", "Clear the search to paste into this folder"]);
        assert!(r.app.copies_only() && r.app.list_takes_no_drops());
        let labels: Vec<&str> = r.app.view_menu_items().iter().map(|m| m.label).collect();
        assert!(labels.contains(&"Show in folder") && !labels.contains(&"Paste") && !labels.contains(&"New folder"), "{labels:?}");
    }

    #[test]
    fn matches_from_several_folders_are_renamed_one_at_a_time() {
        let mut r = search_rig("search-rename-many");
        r.search("notes");
        r.app.select_all();
        r.app.action(Action::Rename);
        assert!(r.app.dialog.is_none(), "no rename-several dialog over matches from several folders");
        assert_eq!(r.app.notices, ["Select one item to rename it"]);
        let rename = r.app.view_menu_items().into_iter().find(|m| m.action == Action::Rename).unwrap();
        assert!(!rename.enabled);
    }

    #[test]
    fn nothing_found_and_a_search_cut_short_say_so() {
        let mut r = search_rig("search-none");
        r.search("zzz");
        assert!(r.app.rows.is_empty());
        assert!(r.app.status_text().starts_with("0 items found"), "{}", r.app.status_text());
        r.key(k::KEY_Escape, "");
        r.pump_until("back", |a| !a.loading);
        for i in 0..gannet_core::search::LIMIT + 5 {
            fs::write(r.work().join(format!("many-{i}")), "").unwrap();
        }
        r.app.reload();
        r.pump_until("read", |a| !a.loading);
        r.search("many");
        assert_eq!(r.app.rows.len(), gannet_core::search::LIMIT);
        assert_eq!(r.app.status_text(), "The first 1,000 found. Type more to narrow it");
    }

    #[test]
    fn the_trash_and_recent_only_filter() {
        let mut r = Rig::new("search-trash-view");
        let t = r.app.trash_dir.clone();
        fs::create_dir_all(&t).unwrap();
        r.open(t);
        r.ctrl(k::KEY_f);
        assert!(matches!(r.app.edit, Edit::Filter(_)));
        assert!(!r.app.search_below, "the Trash has no folders below to search");
        r.ctrl(k::KEY_f);
        assert!(!r.app.search_below, "nor does Ctrl+F in the box turn it into one");
    }

    #[test]
    fn recent_lists_used_files_newest_first_and_only_shows_them() {
        let mut r = Rig::new("recent");
        for (d, n) in [("a", "old.txt"), ("b", "new.txt"), ("a", "middle.txt")] {
            fs::create_dir_all(r.work().join(d)).unwrap();
            fs::write(r.work().join(d).join(n), "x").unwrap();
        }
        let mark = |p: PathBuf, when: &str| format!("<bookmark href=\"{}\" visited=\"{when}\"/>", gannet_core::uri::to_file_uri(&p));
        let xbel = format!(
            "<xbel>{}{}{}{}</xbel>",
            mark(r.work().join("a/old.txt"), "2026-01-01T00:00:00Z"),
            mark(r.work().join("b/new.txt"), "2026-03-01T00:00:00Z"),
            mark(r.work().join("gone.txt"), "2026-04-01T00:00:00Z"),
            mark(r.work().join("a/middle.txt"), "2026-02-01T00:00:00Z"),
        );
        fs::create_dir_all(r.root.join("data")).unwrap();
        fs::write(r.root.join("data/recently-used.xbel"), xbel).unwrap();
        let recent = r.app.recent_file.clone();
        assert_eq!(recent, r.root.join("data/recently-used.xbel"));
        r.open(recent.clone());
        assert!(r.app.is_recent_view());
        let l = r.app.listing.clone().unwrap();
        let names: Vec<_> = r.app.rows.iter().map(|&i| String::from_utf8_lossy(l.name(i)).into_owned()).collect();
        assert_eq!(names, ["new.txt", "middle.txt", "old.txt"], "newest first, with the missing file left out");
        assert_eq!(r.app.crumb_label(&recent), "Recent");
        assert!(r.app.up_target().is_none(), "nothing is above Recent");
        // Its order is fixed: asking for another sort changes nothing.
        r.ctrl(k::KEY_1);
        r.pump_until("settled", |a| !a.loading);
        let names: Vec<_> = r.app.rows.iter().map(|&i| String::from_utf8_lossy(l.name(i)).into_owned()).collect();
        assert_eq!(names, ["new.txt", "middle.txt", "old.txt"]);
        // It only lists files: changing them is refused, and the files stay as they were.
        r.select("middle.txt");
        r.app.action(Action::Trash);
        r.app.action(Action::Rename);
        r.app.action(Action::NewFolder);
        assert!(r.app.jobs.is_empty() && r.app.edit == Edit::None, "nothing started");
        assert!(r.app.notices.iter().any(|n| n.contains("Recent only lists files")), "{:?}", r.app.notices);
        assert!(r.work().join("a/middle.txt").exists());
        // Show in folder goes to the file's folder, with it selected.
        r.app.action(Action::ShowInFolder);
        r.pump_until("its folder", |a| !a.loading && a.dir.ends_with("work/a"));
        assert_eq!(r.selected_names(), ["middle.txt"]);
        r.app.go_back();
        r.pump_until("back to Recent", |a| !a.loading && a.is_recent_view());
        let l = r.app.listing.clone().unwrap();
        assert_eq!(l.name(r.app.rows[r.app.cursor]), b"middle.txt", "on the file it left from");
    }

    #[test]
    fn files_dragged_out_of_recent_are_only_copied_and_never_trashed() {
        let mut r = Rig::new("recent-drag");
        fs::write(r.work().join("a"), "x").unwrap();
        let elsewhere = r.root.join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        let trash = r.root.join("data/Trash/files");
        fs::create_dir_all(&trash).unwrap();
        fs::write(r.root.join("data/recently-used.xbel"), format!("<xbel><bookmark href=\"{}\" visited=\"2026-01-01T00:00:00Z\"/></xbel>", gannet_core::uri::to_file_uri(&r.work().join("a")))).unwrap();
        r.open(r.work());
        // The menu is Recent's from the moment it's shown, before its list arrives.
        r.app.go(Stop { dir: r.app.recent_file.clone(), cursor_name: None, search: None });
        assert!(!r.app.listing.as_ref().is_some_and(|l| l.is_recent()), "Recent's list isn't here yet");
        assert!(r.app.context_items().iter().all(|i| i.action != Action::Paste && i.action != Action::Trash));
        r.pump_until("Recent", |a| !a.loading);
        assert_eq!(r.app.rows.len(), 1);
        r.app.places = vec![
            Place { label: "Elsewhere".into(), path: elsewhere.clone(), kind: PlaceKind::Home, volume: None },
            Place { label: "Recent".into(), path: r.app.recent_file.clone(), kind: PlaceKind::Recent, volume: None },
            Place { label: "Trash".into(), path: trash, kind: PlaceKind::Trash, volume: None },
        ];
        press_on_row(&mut r, 0);
        r.app.input(Input::Move { x: 540.0, y: 300.0 });
        // What the drag's folder is like arrives from another thread.
        r.pump_until("the drag's source", |a| matches!(&a.file_drag, Some(FileDrag::Active(d)) if d.source.is_some()));
        let x = (r.app.layout().sidebar.x + r.app.px(40.0)) as f64;
        let over = |r: &mut Rig, i: usize| {
            r.app.input(Input::Move { x, y: (r.app.place_top(i) + r.app.px(15.0)) as f64 });
            r.pump_until("the drag over the place", |a| matches!(&a.file_drag, Some(FileDrag::Active(d)) if d.devices.values().any(Option::is_some) || i != 0));
            let Some(FileDrag::Active(d)) = &r.app.file_drag else { panic!("no drag") };
            d.target.as_ref().map(|t| r.app.drop_action(d, t))
        };
        assert!(matches!(over(&mut r, 0), Some(DropAction::Copy)), "a copy, even on the same disk");
        assert!(over(&mut r, 1).is_none(), "Recent isn't somewhere to drop");
        assert!(over(&mut r, 2).is_none(), "the Trash isn't a target");
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

    // ---- Screen readers and text fields ----

    fn a11y_edit(r: &mut Rig, id: u64, op: crate::a11y::TextEdit) {
        let edit_gen = r.app.edit_gen;
        r.app.on_msg(Msg::A11y(crate::a11y::Act::EditText(id, edit_gen, op)));
    }

    fn field_text(r: &Rig) -> (String, usize, usize) {
        let f = match &r.app.edit {
            Edit::Path(f) | Edit::Filter(f) | Edit::NewFolder(f) | Edit::Rename { field: f, .. } => f,
            Edit::None => panic!("no field open"),
        };
        (f.text.clone(), f.anchor, f.caret)
    }

    #[test]
    fn a_screen_reader_edits_a_rename_as_typing_would() {
        use crate::a11y::{NAME_FIELD, TextEdit};
        let mut r = Rig::new("a11y-rename");
        fs::write(r.work().join("café.txt"), "x").unwrap();
        r.open(r.work());
        r.select("café.txt");
        r.key(k::KEY_F2, "");
        // Counted in characters: é is two bytes.
        assert_eq!(field_text(&r), ("café.txt".into(), 0, 5), "the stem selected, as for typing");
        a11y_edit(&mut r, NAME_FIELD, TextEdit::Replace(0, 4, "tea\n".into()));
        assert_eq!(field_text(&r), ("tea.txt".into(), 3, 3), "the stem replaced, and no line break in a name");
        a11y_edit(&mut r, NAME_FIELD, TextEdit::Select(4, 7));
        assert_eq!(field_text(&r), ("tea.txt".into(), 4, 7));
        a11y_edit(&mut r, NAME_FIELD, TextEdit::Copy(4, 7));
        assert!(matches!(r.app.cmds.last(), Some(Cmd::SetClipboard(offers)) if offers[0].1 == b"txt"), "copied as Ctrl+C copies");
        a11y_edit(&mut r, NAME_FIELD, TextEdit::Caret(99));
        assert_eq!(field_text(&r), ("tea.txt".into(), 7, 7), "a caret past the end stays at the end");
        // A request about an earlier rename is dropped.
        r.app.on_msg(Msg::A11y(crate::a11y::Act::EditText(NAME_FIELD, r.app.edit_gen - 1, TextEdit::Replace(0, 7, "no".into()))));
        assert_eq!(field_text(&r).0, "tea.txt");
        // A request about a row of another folder is dropped, and leaves the rename alone.
        r.app.on_msg(Msg::A11y(crate::a11y::Act::FocusRow(Arc::new(r.root.clone()), b"work".to_vec())));
        assert_eq!(field_text(&r).0, "tea.txt");
        // Moving to a row ends the rename as clicking there does: it's made.
        let key = r.app.listing.as_ref().unwrap().key(r.app.rows[0]).into_owned();
        r.app.on_msg(Msg::A11y(crate::a11y::Act::FocusRow(Arc::new(r.work()), key)));
        assert!(matches!(r.app.edit, Edit::None));
        r.idle();
        assert!(r.work().join("tea.txt").exists());
    }

    #[test]
    fn a_screen_reader_types_in_the_filter_box() {
        use crate::a11y::{FILTER, PATH_FIELD, TextEdit};
        let mut r = Rig::new("a11y-filter");
        for n in ["apple", "apricot", "banana"] {
            fs::write(r.work().join(n), "x").unwrap();
        }
        r.open(r.work());
        // No box showing: nothing to type in.
        a11y_edit(&mut r, FILTER, TextEdit::Replace(0, 0, "ap".into()));
        assert!(matches!(r.app.edit, Edit::None) && r.app.filter.is_empty());
        r.key(0, "/");
        a11y_edit(&mut r, FILTER, TextEdit::Replace(0, 0, "ap".into()));
        assert_eq!(r.row_names(), ["apple", "apricot"], "the list narrows as it does for typing");
        // Left showing with what it filters by, the box opens again when it's changed.
        r.key(k::KEY_Down, "");
        assert!(matches!(r.app.edit, Edit::None));
        a11y_edit(&mut r, FILTER, TextEdit::Replace(1, 2, "".into()));
        assert_eq!((field_text(&r).0.as_str(), r.row_names().len()), ("a", 3));
        // A path request goes nowhere while the path isn't being typed.
        a11y_edit(&mut r, PATH_FIELD, TextEdit::Replace(0, 1, "/".into()));
        assert_eq!(field_text(&r).0, "a");
        // Moving to the box opens it, as clicking it does.
        r.key(k::KEY_Tab, "");
        r.app.on_msg(Msg::A11y(crate::a11y::Act::Focus(FILTER)));
        assert!(matches!(r.app.edit, Edit::Filter(_)));
    }

    #[test]
    fn a_late_paste_or_request_doesnt_land_in_a_field_opened_since() {
        use crate::a11y::{FILTER, TextEdit};
        let mut r = Rig::new("a11y-late");
        fs::write(r.work().join("a"), "x").unwrap();
        r.open(r.work());
        r.key(0, "/");
        r.ctrl(k::KEY_v);
        let Some(Cmd::ReadClipboard(asked)) = r.app.cmds.pop() else { panic!("no read requested") };
        let stale = r.app.edit_gen;
        // The box closes and opens again before the answers come.
        r.key(k::KEY_Escape, "");
        r.key(0, "/");
        r.app.on_msg(Msg::Clipboard(Some(b"junk".to_vec()), clipboard::TEXT.to_string(), asked));
        r.app.on_msg(Msg::A11y(crate::a11y::Act::EditText(FILTER, stale, TextEdit::Replace(0, 0, "old".into()))));
        assert_eq!(field_text(&r).0, "");
        a11y_edit(&mut r, FILTER, TextEdit::Replace(0, 0, "new".into()));
        assert_eq!(field_text(&r).0, "new");
    }

    #[test]
    fn the_snapshot_has_the_fields_and_where_their_characters_are() {
        use crate::a11y::{FILTER, FRAME, Focus, Link, NAME_FIELD, PATH_FIELD, Role};
        let mut r = Rig::new("a11y-fields");
        fs::write(r.work().join("notes.txt"), "x").unwrap();
        r.open(r.work());
        let mut link = Link::new();
        let s = link.build(&mut r.app);
        assert!(s.get(PATH_FIELD).is_none() && s.get(FILTER).is_none() && s.get(NAME_FIELD).is_none());

        r.ctrl(k::KEY_f);
        let s = link.build(&mut r.app);
        let n = s.get(FILTER).expect("the search box");
        assert_eq!((n.role, n.name.as_str(), n.description.as_str()), (Role::Entry, "Search", "Search here and below"));
        assert_eq!(s.focus, Focus::Node(FILTER));
        r.ctrl(k::KEY_f);
        let s = link.build(&mut r.app);
        let n = s.get(FILTER).unwrap();
        assert_eq!((n.name.as_str(), n.description.as_str()), ("Filter", "Filter this folder"), "Ctrl+F in the box turns it to a filter");
        r.key(k::KEY_Escape, "");

        r.select("notes.txt");
        r.key(k::KEY_F2, "");
        let s = link.build(&mut r.app);
        let n = s.get(NAME_FIELD).expect("the rename field");
        assert_eq!((n.name.as_str(), n.field.as_ref().unwrap().text.as_str()), ("Rename notes.txt", "notes.txt"));
        assert_eq!(s.focus, Focus::Node(NAME_FIELD));
        assert_eq!(s.get(FRAME).unwrap().children.last(), Some(&NAME_FIELD));
        // Its characters are on its row. Tests have no fonts to measure them with: the e2e
        // check sees them left to right.
        let g = s.geometry(NAME_FIELD).unwrap();
        assert_eq!(g.edges.len(), "notes.txt".len() + 1);
        let row = r.app.layout().list;
        assert!(n.rect.y >= row.y && n.rect.bottom() <= row.y + r.app.layout().row_h + 1, "{:?}", n.rect);
        assert!(g.edges[0] >= n.rect.x && *g.edges.last().unwrap() <= n.rect.right());
        r.key(k::KEY_Escape, "");

        r.ctrl(k::KEY_l);
        let s = link.build(&mut r.app);
        let n = s.get(PATH_FIELD).expect("the typed path");
        assert_eq!(n.field.as_ref().unwrap().text, r.work().to_string_lossy());
        assert_eq!(s.focus, Focus::Node(PATH_FIELD));
        assert_eq!(&s.get(FRAME).unwrap().children[..2], &[crate::a11y::BACK, PATH_FIELD]);
    }

    fn type_in(r: &mut Rig, text: &str) {
        for c in text.chars() {
            r.key(0, &c.to_string());
        }
    }

    fn rename_dialog(r: &Rig) -> &RenameMany {
        match &r.app.dialog {
            Some(Dialog::RenameMany(d)) => d,
            _ => panic!("the rename dialog isn't open"),
        }
    }

    #[test]
    fn f2_on_several_items_renames_them_all_and_one_undo_puts_them_back() {
        let mut r = Rig::new("rename-many");
        for (n, body) in [("a.jpg", "1"), ("b.jpg", "2"), ("keep.txt", "3")] {
            fs::write(r.work().join(n), body).unwrap();
        }
        r.open(r.work());
        r.app.input(Input::Key { keysym: k::KEY_a, utf8: "a".into(), mods: Mods { ctrl: true, ..Mods::default() }, repeat: false });
        assert!(r.app.context_items().iter().any(|m| m.label == "Rename" && m.enabled), "offered for several items");
        r.key(k::KEY_F2, "");
        assert!(rename_dialog(&r).find.text.is_empty() && !rename_dialog(&r).can_rename(), "opens with nothing to do yet");
        type_in(&mut r, ".jpg");
        r.key(k::KEY_Tab, "");
        type_in(&mut r, ".jpeg");
        let d = rename_dialog(&r);
        assert_eq!(d.new, [b"a.jpeg".to_vec(), b"b.jpeg".to_vec(), b"keep.txt".to_vec()], "previewed beside the old names");
        assert!(d.summary().0.contains("2 items will be renamed, 1 unchanged"), "{:?}", d.summary());
        r.key(k::KEY_Return, "");
        assert!(r.app.dialog.is_none());
        r.idle();
        assert_eq!(fs::read_to_string(r.work().join("a.jpeg")).unwrap(), "1");
        assert_eq!(fs::read_to_string(r.work().join("b.jpeg")).unwrap(), "2");
        assert_eq!(r.selected_names(), ["a.jpeg", "b.jpeg"], "the renamed items are selected");
        assert_eq!(r.app.undo.len(), 1, "one undo for the lot");
        r.ctrl(k::KEY_z);
        r.idle();
        assert!(r.work().join("a.jpg").exists() && r.work().join("b.jpg").exists() && !r.work().join("a.jpeg").exists());
    }

    #[test]
    fn the_rename_dialog_wont_rename_onto_a_clash_and_says_which() {
        let mut r = Rig::new("rename-many-clash");
        for n in ["a.txt", "b.txt", "c.txt"] {
            fs::write(r.work().join(n), n).unwrap();
        }
        r.open(r.work());
        r.select("a.txt");
        r.app.move_cursor(1, Mods { shift: true, ..Mods::default() });
        r.key(k::KEY_F2, "");
        r.key(k::KEY_Tab, "");
        // Both would be "same.txt".
        type_in(&mut r, "same");
        let (line, problem) = rename_dialog(&r).summary();
        assert!(problem && line.contains("“a.txt” and “b.txt” would both be named “same.txt”"), "{line}");
        r.key(k::KEY_Return, "");
        assert!(r.app.dialog.is_some(), "Enter doesn't rename while names clash");
        // b.txt would take c.txt's name, which isn't being renamed.
        r.key(k::KEY_Tab, "");
        r.key(k::KEY_Tab, "");
        r.key(k::KEY_Tab, "");
        type_in(&mut r, "b");
        r.key(k::KEY_Tab, "");
        r.ctrl(k::KEY_a);
        type_in(&mut r, "c");
        let (line, problem) = rename_dialog(&r).summary();
        assert!(problem && line.contains("“c.txt” is already in this folder"), "{line}");
        // Swapping a and b is fine.
        r.key(k::KEY_Escape, "");
        assert!(r.app.dialog.is_none());
        r.app.start_job(Request::RenameMany { renames: vec![(r.work().join("a.txt"), "b.txt".into()), (r.work().join("b.txt"), "a.txt".into())] });
        r.idle();
        assert_eq!(fs::read_to_string(r.work().join("a.txt")).unwrap(), "b.txt");
        assert_eq!(fs::read_to_string(r.work().join("b.txt")).unwrap(), "a.txt");
        assert_eq!(fs::read_to_string(r.work().join("c.txt")).unwrap(), "c.txt");
    }

    #[test]
    fn pasting_into_the_rename_dialog_updates_the_names() {
        let mut r = Rig::new("rename-many-paste");
        for n in ["x1", "x2"] {
            fs::write(r.work().join(n), "").unwrap();
        }
        r.open(r.work());
        r.app.input(Input::Key { keysym: k::KEY_a, utf8: "a".into(), mods: Mods { ctrl: true, ..Mods::default() }, repeat: false });
        r.key(k::KEY_F2, "");
        r.key(k::KEY_Tab, "");
        r.ctrl(k::KEY_v);
        let Some(Cmd::ReadClipboard(asked)) = r.app.cmds.pop() else { panic!("no read requested") };
        r.app.on_msg(Msg::Clipboard(Some(b"Trip {n}\nsecond line".to_vec()), clipboard::TEXT.to_string(), asked));
        assert_eq!(rename_dialog(&r).replace.text, "Trip {n}");
        assert_eq!(rename_dialog(&r).new, [b"Trip 1".to_vec(), b"Trip 2".to_vec()]);
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
        r.app.input(Input::DragEnter { picture: false, x: 540.0, y, files: true, can_copy: true, can_move: true, marker: None });
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
        r.app.input(Input::DragEnter { picture: false, x: 540.0, y: 200.0, files: true, can_copy: true, can_move: true, marker: None });
        assert!(r.app.file_drag.is_none());
        assert_eq!(r.app.next_deadline(), None);
        r.app.input(Input::DragLeave);
        // The same goes for a press that hasn't become anything yet.
        band_press(&mut r, 2);
        r.app.input(Input::DragEnter { picture: false, x: 540.0, y: 200.0, files: true, can_copy: true, can_move: true, marker: None });
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
        Dropped { paths: names.iter().map(|n| r.root.join(n)).collect(), dest: r.root.join("work"), trash: false, can_copy, can_move, decided: None, all_local: true, saved: None, link: false, pin: None }
    }

    #[test]
    fn a_picture_from_a_browser_is_shown_saved_and_a_link_is_named() {
        let mut r = Rig::new("drop-picture");
        r.open(r.work());
        let saved = r.work().join("columns.png");
        fs::write(&saved, b"png").unwrap();
        let base = Dropped { paths: Vec::new(), dest: r.work(), trash: false, can_copy: true, can_move: false, decided: None, all_local: false, saved: Some(Ok(saved)), link: false, pin: None };
        r.app.cmds.clear();
        r.app.on_msg(Msg::Dropped(base.clone()));
        assert!(r.app.cmds.iter().any(|c| matches!(c, Cmd::FinishDrop)), "the browser hears it was taken");
        r.pump_until("the new file", |a| !a.loading);
        assert_eq!(r.selected_names(), ["columns.png"], "and it's selected");
        // A program that only offered a move isn't told the drop finished, so it keeps its side.
        r.app.cmds.clear();
        r.app.on_msg(Msg::Dropped(Dropped { can_copy: false, can_move: true, ..base.clone() }));
        assert!(r.app.cmds.iter().any(|c| matches!(c, Cmd::AbandonDrop)) && !r.app.cmds.iter().any(|c| matches!(c, Cmd::FinishDrop)), "abandoned, not finished");
        r.app.on_msg(Msg::Dropped(Dropped { saved: Some(Err("no space left on device".into())), ..base.clone() }));
        assert_eq!(r.app.notices.last().map(String::as_str), Some("Couldn't save the picture: no space left on device"));
        r.app.on_msg(Msg::Dropped(Dropped { saved: None, link: true, ..base.clone() }));
        assert_eq!(r.app.notices.last().map(String::as_str), Some("That drag held a link, not a file"));
        r.app.on_msg(Msg::Dropped(Dropped { saved: None, link: false, ..base }));
        assert_eq!(r.app.notices.last().map(String::as_str), Some("Couldn't get any files from that drag"));
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
        r.app.input(Input::DragEnter { picture: false, x: 540.0, y: 500.0, files: true, can_copy: true, can_move: false, marker: Some(marker) });
        r.app.input(Input::DragMove { x: 540.0, y: 500.0 });
        let i = r.app.incoming.as_ref().unwrap();
        assert!(i.devices.contains_key(&r.root.join("work")), "it asked about this folder");
        let (drag, path) = (i.id, r.root.join("work"));
        r.app.on_msg(Msg::DragDevice { drag, path, device });
        r
    }

    #[test]
    fn the_trash_doesnt_light_up_for_a_picture_from_a_browser() {
        let mut r = Rig::new("drop-picture-trash");
        r.open(r.work());
        let trash = r.root.join("data/Trash/files");
        fs::create_dir_all(&trash).unwrap();
        r.app.places = vec![Place { label: "Home".into(), path: r.work(), kind: PlaceKind::Home, volume: None }, Place { label: "Trash".into(), path: trash, kind: PlaceKind::Trash, volume: None }];
        let (x, y) = ((r.app.layout().sidebar.x + r.app.px(40.0)) as f64, (r.app.place_top(1) + r.app.px(15.0)) as f64);
        let over_trash = |r: &mut Rig, picture: bool| {
            r.app.dirty = Rect::default();
            r.app.input(Input::DragEnter { picture, x, y, files: true, can_copy: true, can_move: true, marker: None });
            assert_eq!(r.app.trash_unavailable(), picture, "the Trash is dimmed only when it can't be used");
            assert_eq!(r.app.dirty.contains(r.app.layout().sidebar.x, r.app.place_top(1)), picture, "repainted when that changes");
            r.app.input(Input::DragMove { x, y });
            let target = r.app.incoming.as_ref().unwrap().target.as_ref().map(|t| t.trash);
            r.app.dirty = Rect::default();
            r.app.input(Input::DragLeave);
            assert!(!r.app.trash_unavailable(), "back to normal when it leaves");
            assert!(!picture || r.app.dirty.contains(r.app.layout().sidebar.x, r.app.place_top(1)), "and repainted, as it was dimmed");
            target
        };
        assert_eq!(over_trash(&mut r, false), Some(true), "files can go to the Trash");
        // From another Gannet window, Ctrl asks for a copy, which can't go to the Trash: holding
        // it dims the Trash at once, not only when the drag next comes or goes.
        r.app.has_keyboard = true;
        // Over the list, so the Trash's row isn't repainted for being under the pointer.
        let (lx, ly) = (540.0, 500.0);
        r.app.input(Input::DragEnter { picture: false, x: lx, y: ly, files: true, can_copy: true, can_move: true, marker: Some(marker(true, false, false)) });
        r.app.input(Input::DragMove { x: lx, y: ly });
        assert!(!r.app.trash_unavailable());
        r.app.dirty = Rect::default();
        r.app.input(Input::Mods(Mods { ctrl: true, ..Mods::default() }));
        assert!(r.app.trash_unavailable() && r.app.dirty.contains(r.app.layout().sidebar.x, r.app.place_top(1)), "dimmed and repainted for Ctrl");
        r.app.input(Input::Mods(Mods::default()));
        // Losing focus goes back to the keys the drag set out with, here Ctrl.
        let mut ctrl_marker = marker(true, true, false);
        ctrl_marker.device = 0;
        r.app.input(Input::DragLeave);
        r.app.input(Input::DragEnter { picture: false, x: lx, y: ly, files: true, can_copy: true, can_move: true, marker: Some(ctrl_marker) });
        r.app.input(Input::DragMove { x: lx, y: ly });
        assert!(!r.app.trash_unavailable(), "with the keyboard and no Ctrl held, it can");
        r.app.dirty = Rect::default();
        r.app.input(Input::Focus(false));
        assert!(r.app.trash_unavailable() && r.app.dirty.contains(r.app.layout().sidebar.x, r.app.place_top(1)), "dimmed and repainted when focus goes");
        r.app.input(Input::DragLeave);
        assert_eq!(over_trash(&mut r, true), None, "a picture can't, so nothing lights up");
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
        r.app.input(Input::DragEnter { picture: false, x: 540.0, y: on_folder, files: true, can_copy: true, can_move: true, marker: None });
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
        r.app.input(Input::DragEnter { picture: false, x: 540.0, y: on_folder, files: true, can_copy: true, can_move: true, marker: None });
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
        r.app.input(Input::DragEnter { picture: false, x: 540.0, y: on_folder, files: true, can_copy: true, can_move: true, marker: None });
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
        r.app.input(Input::DragEnter { picture: false, x: 540.0, y: on_folder, files: true, can_copy: true, can_move: true, marker: None });
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
        r.app.input(Input::DragEnter { picture: false, x: 540.0, y: 500.0, files: true, can_copy: true, can_move: true, marker: None });
        r.app.input(Input::DragMove { x: 540.0, y: 500.0 });
        r.app.input(Input::DragLeave);
        r.app.input(Input::DragEnter { picture: false, x: 540.0, y: on_folder, files: true, can_copy: true, can_move: true, marker: None });
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
        r.app.input(Input::DragEnter { picture: false, x: 540.0, y: 500.0, files: true, can_copy: true, can_move: true, marker: None });
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
        r.app.input(Input::DragEnter { picture: false, x: 540.0, y: y as f64, files: true, can_copy: true, can_move: true, marker: None });
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
        r.app.input(Input::DragEnter { picture: false, x: 540.0, y: 500.0, files: true, can_copy: true, can_move: true, marker: None });
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
        r.app.input(Input::DragEnter { picture: false, x: 540.0, y, files: true, can_copy: true, can_move: true, marker: None });
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
        r.app.input(Input::DragEnter { picture: false, x: 540.0, y: 500.0, files: false, can_copy: true, can_move: true, marker: None });
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
        r.app.input(Input::DragEnter { picture: false, x: 540.0, y: row(0), files: true, can_copy: true, can_move: true, marker: None });
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
        r.app.input(Input::DragEnter { picture: false, x: 540.0, y: 500.0, files: true, can_copy: true, can_move: true, marker: None });
        assert!(r.app.incoming.as_ref().is_some_and(|i| i.target.is_none()));
    }

    #[test]
    fn a_chooser_takes_no_drops_from_outside_or_from_its_own_list() {
        let mut r = drag_rig("chooser-drops");
        fs::create_dir(r.root.join("work/d")).unwrap();
        r.app.chooser = Some(crate::chooser::Chooser::new(crate::chooser::Spec::default(), &[], None));
        r.app.input(Input::Key { keysym: k::KEY_F5, utf8: String::new(), mods: Mods::default(), repeat: false });
        r.pump_until("the folder", |a| a.rows.len() == 3);
        let l = r.app.layout();
        let on_folder = (l.list.y + l.row_h / 2) as f64;
        r.app.input(Input::DragEnter { picture: false, x: 540.0, y: on_folder, files: true, can_copy: true, can_move: true, marker: None });
        r.app.input(Input::DragMove { x: 540.0, y: on_folder });
        assert!(r.app.incoming.as_ref().is_some_and(|i| i.target.is_none()), "nothing from outside");
        r.app.input(Input::DragLeave);
        press_on_row(&mut r, 1);
        r.app.input(Input::Move { x: 540.0, y: on_folder });
        let Some(FileDrag::Active(d)) = &r.app.file_drag else { panic!("no drag") };
        assert!(d.target.is_none(), "nor from its own list onto a folder");
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
    fn a_chooser_doesnt_cut_paste_or_delete_for_good_by_key() {
        let mut r = Rig::new("chooser-keys");
        fs::write(r.work().join("a.txt"), "x").unwrap();
        fs::write(r.work().join("b.txt"), "y").unwrap();
        r.app.chooser = Some(crate::chooser::Chooser::new(crate::chooser::Spec::default(), &[], None));
        r.open(r.work());
        r.select("a.txt");
        r.ctrl(k::KEY_x);
        r.ctrl(k::KEY_c);
        r.ctrl(k::KEY_v);
        assert!(!r.app.cmds.iter().any(|c| matches!(c, Cmd::ReadClipboard(_) | Cmd::SetClipboard(_))), "no clipboard");
        r.app.key(k::KEY_Delete, "", Mods { shift: true, ..Mods::default() });
        assert!(r.app.dialog.is_none(), "Shift+Delete doesn't offer to delete for good");
        // Moving to the Trash is still there, by key and in the menu.
        assert!(r.app.context_items().iter().any(|i| i.action == Action::Trash && i.enabled));
        r.key(k::KEY_Delete, "");
        r.idle();
        assert!(!r.work().join("a.txt").exists() && r.root.join("data/Trash/files/a.txt").exists());
        // In the Trash, reached by a typed path, Delete would mean delete for good.
        r.open(r.root.join("data/Trash/files"));
        r.select("a.txt");
        r.key(k::KEY_Delete, "");
        assert!(r.app.dialog.is_none() && r.root.join("data/Trash/files/a.txt").exists());
        assert!(!r.app.context_items().iter().any(|i| i.action == Action::Trash && i.enabled));
    }

    #[test]
    fn a_chooser_can_undo_and_redo() {
        let mut r = Rig::new("chooser-redo");
        r.app.chooser = Some(crate::chooser::Chooser::new(crate::chooser::Spec::default(), &[], None));
        r.open(r.work());
        r.app.start_job(Request::NewFolder { parent: r.work(), name: "made".into() });
        r.idle();
        r.ctrl(k::KEY_z);
        r.idle();
        assert!(!r.work().join("made").exists());
        r.app.key(k::KEY_Z, "", Mods { ctrl: true, shift: true, ..Mods::default() });
        r.idle();
        assert!(r.work().join("made").is_dir());
    }

    #[test]
    fn saving_as_dot_dot_goes_up_a_folder_by_its_real_name() {
        let mut r = Rig::new("chooser-dotdot");
        fs::create_dir(r.work().join("sub")).unwrap();
        let spec = crate::chooser::Spec { mode: crate::chooser::Mode::Save, name: Some("x.txt".into()), ..crate::chooser::Spec::default() };
        r.app.chooser = Some(crate::chooser::Chooser::new(spec, &[], None));
        r.open(r.work().join("sub"));
        if let Some(c) = r.app.chooser.as_mut() {
            c.name = Field::new("..");
            c.name_focused = true;
        }
        r.key(k::KEY_Return, "");
        r.pump_until("the folder above", |a| !a.loading);
        assert_eq!(r.app.dir, r.work());
    }

    #[test]
    fn ctrl_shift_c_copies_the_path_and_ctrl_shift_z_doesnt_undo() {
        let mut r = Rig::new("shift-keys");
        fs::write(r.work().join("a"), "").unwrap();
        r.open(r.work());
        r.select("a");
        r.app.cmds.clear();
        let shift = Mods { ctrl: true, shift: true, ..Mods::default() };
        r.app.key(k::KEY_C, "", shift);
        let Some(Cmd::SetClipboard(offers)) = r.app.cmds.pop() else { panic!("nothing copied") };
        assert!(offers.iter().all(|(m, _)| *m != clipboard::GNOME), "text only, not a file copy");
        r.app.undo.push(Undo(Vec::new()));
        r.app.key(k::KEY_Z, "", shift);
        r.app.key(k::KEY_z, "", shift);
        assert_eq!(r.app.undo.len(), 1, "Ctrl+Shift+Z must not undo");
    }

    #[test]
    fn ctrl_shift_z_redoes_what_ctrl_z_undid_until_something_new_happens() {
        let mut r = Rig::new("redo");
        r.open(r.work());
        let shift = Mods { ctrl: true, shift: true, ..Mods::default() };
        r.app.start_job(Request::NewFolder { parent: r.work(), name: "made".into() });
        r.idle();
        assert_eq!((r.app.undo.len(), r.app.redo.len()), (1, 0));
        r.ctrl(k::KEY_z);
        r.idle();
        assert!(!r.work().join("made").exists());
        assert_eq!((r.app.undo.len(), r.app.redo.len()), (0, 1));
        r.app.key(k::KEY_Z, "", shift);
        r.idle();
        assert!(r.work().join("made").is_dir());
        assert_eq!((r.app.undo.len(), r.app.redo.len()), (1, 0), "a redo can be undone again");
        r.ctrl(k::KEY_z);
        r.idle();
        assert_eq!(r.app.redo.len(), 1);
        r.app.start_job(Request::NewFolder { parent: r.work(), name: "other".into() });
        r.idle();
        assert_eq!((r.app.undo.len(), r.app.redo.len()), (1, 0), "something new clears redo");
        assert!(r.app.context_items().iter().any(|m| m.label == "Redo" && !m.enabled));
        // An undo still running when something new starts doesn't offer a redo when it ends.
        r.ctrl(k::KEY_z);
        r.app.start_job(Request::NewFolder { parent: r.work(), name: "third".into() });
        r.idle();
        assert!(!r.work().join("other").exists() && r.work().join("third").is_dir());
        assert_eq!(r.app.redo.len(), 0, "something new started during the undo clears redo");
    }

    #[test]
    fn a_copy_or_link_with_no_undo_ends_the_older_undo() {
        let mut r = Rig::new("ends-undo");
        fs::write(r.work().join("a"), "").unwrap();
        fs::create_dir(r.work().join("sub")).unwrap();
        r.open(r.work());
        r.app.start_job(Request::Rename { from: r.work().join("a"), to_name: "b".into() });
        r.idle();
        assert_eq!(r.app.undo.len(), 1);
        // A copy that made nothing (here its source is gone) leaves no undo, so Ctrl+Z right
        // after mustn't rename b back. A merge is the same.
        r.app.start_job(Request::Copy { sources: vec![r.work().join("gone")], dest: r.work().join("sub") });
        r.idle();
        assert!(r.app.undo.is_empty(), "nothing older to undo after a change that can't be undone");
        // The same for a link.
        r.app.start_job(Request::Rename { from: r.work().join("b"), to_name: "c".into() });
        r.idle();
        assert_eq!(r.app.undo.len(), 1);
        r.app.start_job(Request::Link { sources: vec![r.work().join("c")], dest: r.work().join("no-such-folder") });
        r.idle();
        assert!(r.app.undo.is_empty(), "a link that made nothing ends undo too");
        // A copy that can be undone goes on the list as usual.
        r.app.start_job(Request::Copy { sources: vec![r.work().join("c")], dest: r.work().join("sub") });
        r.idle();
        assert_eq!(r.app.undo.len(), 1);
    }

    #[test]
    fn an_undo_that_stops_partway_says_how_far_it_got() {
        let mut r = Rig::new("undo-partway");
        fs::write(r.work().join("a"), "1").unwrap();
        fs::write(r.work().join("b"), "2").unwrap();
        r.open(r.work());
        r.app.start_job(Request::Trash { paths: vec![r.work().join("a"), r.work().join("b")] });
        r.idle();
        fs::write(r.work().join("b"), "took its name").unwrap();
        r.ctrl(k::KEY_z);
        r.idle();
        let notices = r.app.notices.clone();
        assert!(notices.iter().any(|n| n == "Undid 1 of 2 changes. The others were left as they were"), "{notices:?}");
        assert!(notices.iter().any(|n| n.starts_with("b: ") && n.contains("stayed in the Trash")), "{notices:?}");
        assert_eq!(r.app.redo.len(), 1, "the part that was undone can be redone");
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

    /// The name of the file the preview window is showing, as the list knows it.
    fn previewing(r: &Rig) -> Option<String> {
        r.app.previewer.as_ref().map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
    }

    fn with_mods(r: &mut Rig, sym: u32, utf8: &str, mods: Mods) {
        r.app.input(Input::Key { keysym: sym, utf8: utf8.into(), mods, repeat: false });
    }

    /// The names the list asked a preview window to open, or to show, since the last look.
    fn sent(r: &mut Rig) -> Vec<String> {
        let name = |p: &PathBuf| p.file_name().unwrap().to_string_lossy().into_owned();
        r.app
            .cmds
            .drain(..)
            .filter_map(|c| match c {
                Cmd::OpenPreview { path, .. } => Some(format!("open {}", name(&path))),
                Cmd::PreviewShow(path, _) => Some(format!("show {}", name(&path))),
                Cmd::ClosePreview => Some("close".to_string()),
                Cmd::PreviewStepped(path) => Some(format!("stepped {}", name(&path))),
                Cmd::Quit => Some("quit".to_string()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn space_opens_the_preview_window_and_closes_it_and_leaves_the_selection_alone() {
        let mut r = preview_rig("space");
        r.app.cmds.clear();
        r.key(k::KEY_space, " ");
        assert_eq!(sent(&mut r), ["open a"]);
        assert_eq!(previewing(&r).as_deref(), Some("a"));
        assert_eq!(r.selected_names(), ["a"], "showing the first row selects it, as arrows do");
        r.key(k::KEY_space, " ");
        assert_eq!(sent(&mut r), ["close"]);
        assert_eq!(previewing(&r), None);
        assert_eq!(r.selected_names(), ["a"]);
        r.key(k::KEY_space, " ");
        r.key(k::KEY_Escape, "");
        assert_eq!(sent(&mut r), ["open a", "close"]);
        assert_eq!(r.selected_names(), ["a"], "Escape closes the preview first, and nothing else");
    }

    #[test]
    fn each_folder_opens_in_the_order_it_was_last_sorted() {
        let mut r = Rig::new("folder-sorts");
        r.app.folder_sorts = FolderSorts::new(r.root.join("state/gannet/sorts"), vec![r.work().join("dl")]);
        for d in ["a/inner", "b", "dl/inner"] {
            fs::create_dir_all(r.work().join(d)).unwrap();
        }
        let newest = SortSpec { key: SortKey::Modified, descending: true };
        let biggest = SortSpec { key: SortKey::Size, descending: true };
        r.open(r.work().join("a"));
        assert_eq!(r.app.sort, SortSpec::default());
        // The first press is newest first, the next reverses it, and the next newest first again.
        r.ctrl(k::KEY_3);
        assert_eq!(r.app.sort, newest);
        r.ctrl(k::KEY_3);
        assert_eq!(r.app.sort, SortSpec { key: SortKey::Modified, descending: false });
        r.ctrl(k::KEY_2);
        assert_eq!(r.app.sort, biggest);
        r.ctrl(k::KEY_1);
        r.ctrl(k::KEY_2);
        r.open(r.work().join("b"));
        assert_eq!(r.app.sort, SortSpec::default());
        r.open(r.work().join("a/inner"));
        assert_eq!(r.app.sort, biggest, "inherited from the folder above");
        r.open(r.work().join("dl/inner"));
        assert_eq!(r.app.sort, newest, "under Downloads");
        // The Recent view neither changes nor saves an order.
        let recent = r.app.recent_file.clone();
        r.open(recent);
        r.ctrl(k::KEY_1);
        assert_eq!(r.app.view_sort(), newest);
        r.open(r.work().join("a"));
        assert_eq!(r.app.sort, biggest);
    }

    #[test]
    fn the_window_opens_sorted_and_filtered_as_the_list_is() {
        let mut r = preview_rig("settings");
        r.app.show_hidden = true;
        r.app.sort = SortSpec { key: SortKey::Size, descending: true };
        r.app.cmds.clear();
        r.key(k::KEY_space, " ");
        let Some(Cmd::OpenPreview { hidden, sort, .. }) = r.app.cmds.pop() else { panic!("no window") };
        assert!(hidden);
        assert_eq!(sort, SortSpec { key: SortKey::Size, descending: true });
    }

    #[test]
    fn ctrl_space_toggles_selection_without_a_preview() {
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
        assert!(sent(&mut r).is_empty());
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
    fn moving_in_the_list_tells_the_window_once() {
        let mut r = preview_rig("follow");
        r.key(k::KEY_space, " ");
        r.app.cmds.clear();
        for _ in 0..3 {
            with_mods(&mut r, k::KEY_Down, "", Mods::default());
        }
        assert_eq!(sent(&mut r), ["show b", "show c"], "the end stays put, and isn't sent again");
        // The list can do everything else while the window is up.
        r.key(k::KEY_F2, "");
        assert!(matches!(r.app.edit, Edit::Rename { .. }));
    }

    #[test]
    fn the_windows_steps_move_the_cursor() {
        let mut r = preview_rig("stepped");
        r.key(k::KEY_space, " ");
        let generation = r.app.previewer_gen;
        r.app.cmds.clear();
        r.app.on_msg(Msg::PreviewStepped { generation, path: r.work().join("c") });
        assert_eq!(r.app.cursor, 2);
        assert_eq!(r.selected_names(), ["c"], "the selection follows, so Enter opens what's showing");
        assert!(sent(&mut r).is_empty(), "and nothing is sent back");
        // From a window that's since been replaced, or a folder the list has left.
        r.app.on_msg(Msg::PreviewStepped { generation: generation - 1, path: r.work().join("a") });
        r.app.on_msg(Msg::PreviewStepped { generation, path: r.root.join("elsewhere/a") });
        assert_eq!(r.app.cursor, 2);
    }

    #[test]
    fn a_step_to_a_file_the_list_hides_isnt_undone() {
        let mut r = preview_rig("filtered");
        fs::write(r.work().join("ab"), "x").unwrap();
        r.app.reload();
        r.pump_until("the new file", |a| !a.loading && a.rows.len() == 4);
        r.app.filter = "a".into();
        r.app.rederive(false);
        r.pump_until("filtered", |a| a.rows.len() == 2);
        r.key(k::KEY_space, " ");
        let generation = r.app.previewer_gen;
        r.app.cmds.clear();
        // The window walks on to "b", which the filter hides from the list.
        r.app.on_msg(Msg::PreviewStepped { generation, path: r.work().join("b") });
        assert_eq!(r.app.cursor, 0, "the list's cursor stays");
        assert!(sent(&mut r).is_empty(), "and doesn't pull the window back");
        assert_eq!(previewing(&r).as_deref(), Some("b"));
        // Moving in the list is still news.
        with_mods(&mut r, k::KEY_Down, "", Mods::default());
        assert_eq!(sent(&mut r), ["show ab"]);
    }

    #[test]
    fn the_menus_preview_brings_the_window_back_to_the_cursor() {
        let mut r = preview_rig("menu-back");
        r.key(k::KEY_space, " ");
        let generation = r.app.previewer_gen;
        // The window walked off to something the list doesn't follow.
        r.app.on_msg(Msg::PreviewStepped { generation, path: r.root.join("elsewhere/x") });
        r.app.cmds.clear();
        r.app.action(Action::Preview);
        assert_eq!(sent(&mut r), ["show a"]);
        r.app.action(Action::Preview);
        assert!(sent(&mut r).is_empty(), "and only once");
    }

    #[test]
    fn stepping_doesnt_lose_a_selection_of_several() {
        let mut r = preview_rig("several");
        r.app.select_all();
        r.key(k::KEY_space, " ");
        let generation = r.app.previewer_gen;
        r.app.on_msg(Msg::PreviewStepped { generation, path: r.work().join("b") });
        assert_eq!(r.app.cursor, 1);
        assert_eq!(r.selected_names(), ["a", "b", "c"]);
    }

    #[test]
    fn a_window_that_ends_is_forgotten_and_a_crash_is_reported() {
        let mut r = preview_rig("ended");
        r.key(k::KEY_space, " ");
        let generation = r.app.previewer_gen;
        r.app.on_msg(Msg::PreviewEnded { generation: generation - 1, crashed: true });
        assert!(r.app.previewer.is_some(), "an older window's ending isn't this one's");
        r.app.on_msg(Msg::PreviewEnded { generation, crashed: false });
        assert!(r.app.previewer.is_none());
        assert!(r.app.notices.is_empty(), "closing it from the window is ordinary");
        r.key(k::KEY_space, " ");
        r.app.on_msg(Msg::PreviewEnded { generation: r.app.previewer_gen, crashed: true });
        assert!(r.app.previewer.is_none());
        assert!(r.app.notices.iter().any(|n| n.contains("stopped unexpectedly")), "{:?}", r.app.notices);
        r.app.cmds.clear();
        r.key(k::KEY_space, " ");
        assert_eq!(sent(&mut r), ["open a"], "and Space opens a new one");
    }

    #[test]
    fn nothing_to_preview_in_an_empty_folder() {
        let mut r = Rig::new("empty");
        r.open(r.work());
        r.app.cmds.clear();
        r.key(k::KEY_space, " ");
        assert_eq!(previewing(&r), None);
        assert!(sent(&mut r).is_empty());
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
    fn the_context_menu_previews_and_doesnt_close_an_open_window() {
        let mut r = preview_rig("menu");
        r.key(k::KEY_Down, "");
        r.app.action(Action::Preview);
        assert_eq!(previewing(&r).as_deref(), Some("a"));
        r.key(k::KEY_Down, "");
        r.app.cmds.clear();
        r.app.action(Action::Preview);
        assert_eq!(previewing(&r).as_deref(), Some("b"));
        assert!(!sent(&mut r).contains(&"close".to_string()));
    }

    // ---- The preview window itself ----

    /// The preview window on `name` in a folder of plain files, its listing read.
    fn viewer_rig(tag: &str, name: &str) -> Rig {
        let mut r = Rig::new(tag);
        for name in ["a", "b", "c"] {
            fs::write(r.work().join(name), "x").unwrap();
        }
        r.app.start_viewer(r.work().join(name));
        r.pump_until("folder loaded", |a| !a.loading);
        r
    }

    fn showing(r: &Rig) -> Option<String> {
        r.app.preview.as_ref().map(|p| p.path.file_name().unwrap().to_string_lossy().into_owned())
    }

    #[test]
    fn the_window_shows_its_file_at_once_then_walks_its_folder() {
        let mut r = Rig::new("viewer-start");
        for name in ["a", "b", "c"] {
            fs::write(r.work().join(name), "x").unwrap();
        }
        r.app.start_viewer(r.work().join("b"));
        assert_eq!(showing(&r).as_deref(), Some("b"), "before the folder is read");
        assert!(matches!(r.app.cmds.last(), Some(Cmd::Title(t)) if t == "b"), "named for the file");
        r.pump_until("folder loaded", |a| !a.loading);
        assert_eq!(showing(&r).as_deref(), Some("b"), "the cursor landed on it");
        r.app.cmds.clear();
        r.key(k::KEY_Down, "");
        r.key(k::KEY_Right, "");
        assert_eq!(showing(&r).as_deref(), Some("c"));
        r.key(k::KEY_Up, "");
        r.key(k::KEY_Left, "");
        r.key(k::KEY_Left, "");
        assert_eq!(showing(&r).as_deref(), Some("a"));
        assert_eq!(sent(&mut r), ["stepped c", "stepped b", "stepped a"], "each step is told to the list, and the ends stay put");
    }

    fn row_names(r: &Rig) -> Vec<String> {
        let l = r.app.listing.as_ref().unwrap();
        r.app.rows.iter().map(|&i| String::from_utf8_lossy(l.name(i)).into_owned()).collect()
    }

    #[test]
    fn the_window_walks_in_the_lists_order() {
        let z_a = SortSpec { key: SortKey::Name, descending: true };
        let mut r = viewer_rig("viewer-order", "a");
        r.app.fixed_sort = true;
        // The list went to a folder sorted Z to A, on the same file.
        r.app.on_msg(Msg::PreviewShow(r.work().join("a"), z_a));
        r.pump_until("sorted again", |a| !a.loading && a.sort == z_a && a.rows.len() == 3);
        r.idle();
        assert_eq!(row_names(&r), ["c", "b", "a"]);
        assert_eq!(r.app.cursor, 2, "still on its file");
        // The list's order wins over anything saved for the folder.
        r.app.folder_sorts.set(&r.work(), SortSpec::default());
        r.app.on_msg(Msg::PreviewShow(r.work().join("b"), z_a));
        r.idle();
        assert_eq!(r.app.sort, z_a);
        assert_eq!(showing(&r).as_deref(), Some("b"));
    }

    #[test]
    fn a_window_on_its_own_takes_the_folders_saved_order() {
        let z_a = SortSpec { key: SortKey::Name, descending: true };
        let mut r = Rig::new("viewer-own-order");
        for name in ["a", "b", "c"] {
            fs::write(r.work().join(name), "x").unwrap();
        }
        r.app.folder_sorts.set(&r.work(), z_a);
        r.app.start_viewer(r.work().join("a"));
        r.pump_until("folder loaded", |a| !a.loading);
        r.idle();
        assert_eq!(r.app.sort, z_a);
        assert_eq!(row_names(&r), ["c", "b", "a"]);
    }

    #[test]
    fn sorting_the_list_tells_an_open_window() {
        let mut r = Rig::new("list-sort-tells");
        for name in ["a", "b"] {
            fs::write(r.work().join(name), "x").unwrap();
        }
        r.app.go(Stop { dir: r.work(), cursor_name: None, search: None });
        r.pump_until("loaded", |a| !a.loading);
        r.app.previewer = Some(r.work().join("a"));
        r.app.cmds.clear();
        r.app.action(Action::SortBy(SortKey::Modified));
        assert!(r.app.cmds.iter().any(|c| matches!(c, Cmd::PreviewShow(p, s) if p.ends_with("a") && *s == SortKey::Modified.first_order())));
    }

    #[test]
    fn sorting_the_list_leaves_a_window_on_another_folder_alone() {
        let mut r = Rig::new("list-sort-elsewhere");
        fs::create_dir(r.work().join("other")).unwrap();
        fs::write(r.work().join("other/a"), "x").unwrap();
        r.app.go(Stop { dir: r.work(), cursor_name: None, search: None });
        r.pump_until("loaded", |a| !a.loading);
        r.app.previewer = Some(r.work().join("other/a"));
        r.app.cmds.clear();
        r.app.action(Action::SortBy(SortKey::Size));
        assert!(!r.app.cmds.iter().any(|c| matches!(c, Cmd::PreviewShow(..))), "the window's folder isn't the one sorted");
    }

    #[test]
    fn the_window_follows_the_list_without_echoing() {
        let mut r = viewer_rig("viewer-follow", "a");
        r.app.cmds.clear();
        r.app.on_msg(Msg::PreviewShow(r.work().join("c"), SortSpec::default()));
        assert_eq!(showing(&r).as_deref(), Some("c"));
        assert_eq!(r.app.cursor, 2, "its own cursor moves too, for the arrows to go on from");
        assert!(sent(&mut r).is_empty(), "the list already knows");
        // The list went to another folder.
        let other = r.root.join("other");
        fs::create_dir_all(&other).unwrap();
        fs::write(other.join("z"), "x").unwrap();
        r.app.on_msg(Msg::PreviewShow(other.join("z"), SortSpec::default()));
        assert_eq!(showing(&r).as_deref(), Some("z"));
        let generation = r.app.preview_gen;
        r.pump_until("the other folder", |a| !a.loading && a.dir == other);
        r.pump_until("its size", |a| a.listing.as_ref().is_some_and(|l| a.rows.first().is_some_and(|&i| matches!(l.stat(i), Stat::Known { .. }))));
        r.idle();
        assert_eq!(showing(&r).as_deref(), Some("z"));
        assert_eq!(r.app.preview_gen, generation, "the folder arriving doesn't make it read the file again");
        assert!(sent(&mut r).iter().all(|c| !c.starts_with("stepped")));
    }

    #[test]
    fn a_hidden_file_is_shown_and_walked_from() {
        let mut r = Rig::new("viewer-hidden");
        for name in [".h", "a"] {
            fs::write(r.work().join(name), "x").unwrap();
        }
        r.app.start_viewer(r.work().join(".h"));
        r.pump_until("folder loaded", |a| !a.loading);
        assert_eq!(showing(&r).as_deref(), Some(".h"), "the listing didn't move it to another file");
        r.key(k::KEY_Down, "");
        assert_eq!(showing(&r).as_deref(), Some("a"));
    }

    #[test]
    fn space_escape_and_the_list_going_all_close_the_window() {
        for (tag, how) in [("viewer-space", 0), ("viewer-escape", 1), ("viewer-gone", 2), ("viewer-ctrl-w", 3)] {
            let mut r = viewer_rig(tag, "a");
            r.app.cmds.clear();
            match how {
                0 => r.key(k::KEY_space, " "),
                1 => r.key(k::KEY_Escape, ""),
                2 => r.app.on_msg(Msg::ListGone),
                _ => with_mods(&mut r, k::KEY_w, "w", Mods { ctrl: true, ..Mods::default() }),
            }
            assert_eq!(sent(&mut r), ["quit"], "{tag}");
        }
    }

    #[test]
    fn other_keys_do_nothing_in_the_window() {
        let mut r = viewer_rig("viewer-keys", "a");
        r.key(k::KEY_c, "c");
        r.key(k::KEY_Delete, "");
        r.key(k::KEY_F2, "");
        assert_eq!(r.app.cursor, 0, "typing didn't jump");
        assert!(r.app.jobs.is_empty(), "Delete didn't trash");
        assert!(matches!(r.app.edit, Edit::None), "F2 didn't rename");
        assert!(r.work().join("a").exists());
    }

    #[test]
    fn a_file_removed_underneath_moves_on_and_an_empty_folder_closes() {
        let mut r = viewer_rig("removed", "a");
        fs::remove_file(r.work().join("a")).unwrap();
        r.app.reload();
        r.pump_until("the next file", |a| a.preview.as_ref().is_some_and(|p| p.path.ends_with("b")));
        for name in ["b", "c"] {
            fs::remove_file(r.work().join(name)).unwrap();
        }
        r.app.cmds.clear();
        r.app.reload();
        r.pump_until("the window to close", |a| a.cmds.iter().any(|c| matches!(c, Cmd::Quit)));
    }

    #[test]
    fn a_file_overwritten_while_showing_is_read_again() {
        let mut r = viewer_rig("overwritten", "a");
        r.pump_until("a size for the first row", |a| a.listing.as_ref().is_some_and(|l| matches!(l.stat(a.rows[0]), Stat::Known { .. })));
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
        assert_eq!(showing(&r).as_deref(), Some("a"));
    }

    #[test]
    fn a_report_from_before_a_key_press_doesnt_undo_it() {
        use crate::media::{PlayState, parts_for_test};
        use crate::preview::{Content, Event, Found};
        let mut r = viewer_rig("stale-report", "a");
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
    fn an_animation_steps_through_its_frames_in_time_and_round_again() {
        use crate::preview::{Content, Event, Found, ImageBuf};
        let mut r = viewer_rig("animation", "a");
        let generation = r.app.preview_gen;
        let image = Arc::new(ImageBuf::animation_for_test(&[100, 300]));
        let start = Instant::now();
        r.app.on_msg(Msg::Preview(Event { generation, found: Found::Full { image, size: 1 } }));
        let frame = |r: &Rig| r.app.preview.as_ref().unwrap().frame;
        assert_eq!(frame(&r), 0);
        let due = r.app.next_deadline().unwrap();
        assert!(due >= start + Duration::from_millis(100) && due <= Instant::now() + Duration::from_millis(100), "the first frame's time");
        r.app.on_timer(due - Duration::from_millis(1));
        assert_eq!(frame(&r), 0, "not yet");
        r.app.on_timer(due);
        assert_eq!(frame(&r), 1);
        assert_eq!(r.app.next_deadline(), Some(due + Duration::from_millis(300)), "timed from when it was due, so it doesn't drift");
        r.app.on_timer(due + Duration::from_millis(300));
        assert_eq!(frame(&r), 0, "round again");
        // Asleep for a minute: it carries on from now, rather than racing to catch up.
        let late = due + Duration::from_secs(60);
        r.app.on_timer(late);
        assert_eq!(frame(&r), 1);
        assert_eq!(r.app.next_deadline(), Some(late + Duration::from_millis(300)));
        assert!(matches!(r.app.preview.as_ref().unwrap().content, Content::Image { .. }));
    }

    #[test]
    fn a_scroll_before_the_first_layout_isnt_lost() {
        use crate::preview::{Event, Found, Info};
        let mut r = viewer_rig("early-scroll", "a");
        let generation = r.app.preview_gen;
        let text = (0..2000).map(|i| format!("line {i}\n")).collect::<String>();
        let info = Info { mime: "text/plain".into(), description: String::new(), icon: crate::icons::Icon::Document, size: None, modified: None, thumbnail: None, folder: None };
        r.app.on_msg(Msg::Preview(Event { generation, found: Found::Doc { doc: crate::doc::plain(text, false), info } }));
        r.key(k::KEY_End, "");
        let (w, h) = (r.app.w, r.app.h);
        let mut pixels = vec![0u32; (w * h) as usize];
        let mut canvas = crate::canvas::Canvas::new(&mut pixels, w, h);
        canvas.set_clip(Rect::new(0, 0, w, h));
        r.app.paint(&mut canvas);
        let p = r.app.preview.as_ref().unwrap();
        let height = p.doc_layout.as_ref().unwrap().height;
        assert!(height > h, "longer than the window");
        assert!(p.scroll > (height - h) as f64 - 1.0, "at the end: {} of {height}", p.scroll);
    }

    #[test]
    fn a_pdf_keeps_its_place_and_its_pages_through_a_resize() {
        use crate::preview::{Content, Event, Found, Info};
        use crate::tools::Page;
        let mut r = viewer_rig("pdf-resize", "a");
        let generation = r.app.preview_gen;
        let info = Info { mime: "application/pdf".into(), description: String::new(), icon: crate::icons::Icon::Document, size: None, modified: None, thumbnail: None, folder: None };
        let pages = vec![Page { w: 612.0, h: 792.0, turned: false }; 20];
        r.app.on_msg(Msg::Preview(Event { generation, found: Found::Pdf { pages, info } }));
        let pdf = |r: &Rig| match &r.app.preview.as_ref().unwrap().content {
            Content::Pdf(pdf) => (pdf.drawing_page(), r.app.preview.as_ref().unwrap().scroll),
            _ => panic!("not a PDF"),
        };
        assert_eq!(pdf(&r).0, Some(0), "the first page is drawn first");
        r.key(k::KEY_End, "");
        let (drawing, at_end) = pdf(&r);
        assert_eq!(drawing, Some(19), "then the last, once it's scrolled to");
        // Half as wide: the pages are half as tall, so the old scroll is past the new end.
        let (w, h) = (r.app.w, r.app.h);
        r.app.resized(w / 2, h, 1.0);
        let (drawing, scroll) = pdf(&r);
        assert!(scroll < at_end * 0.6 && scroll > at_end * 0.4, "the same place, at the new size: {scroll} from {at_end}");
        assert_eq!(drawing, Some(19), "still drawing what's in view, at the new width");
    }

    #[test]
    fn scrolling_a_pdf_repaints_the_page_number_below_it() {
        use crate::preview::{Event, Found, Info};
        use crate::tools::Page;
        let mut r = viewer_rig("pdf-footer", "a");
        let generation = r.app.preview_gen;
        let info = Info { mime: "application/pdf".into(), description: String::new(), icon: crate::icons::Icon::Document, size: None, modified: None, thumbnail: None, folder: None };
        r.app.on_msg(Msg::Preview(Event { generation, found: Found::Pdf { pages: vec![Page { w: 612.0, h: 792.0, turned: false }; 10], info } }));
        // The footer is the whole width of the bottom of the window.
        let footer = |d: Rect, r: &Rig| d.bottom() >= r.app.h && d.x <= 0 && d.right() >= r.app.w;
        r.app.dirty = Rect::default();
        r.key(k::KEY_Page_Down, "");
        assert!(footer(r.app.dirty, &r), "by a key: {:?}", r.app.dirty);
        r.app.dirty = Rect::default();
        r.app.input(Input::Scroll { lines: 1.0, pixels: 0.0 });
        assert!(footer(r.app.dirty, &r), "and by the wheel: {:?}", r.app.dirty);
    }

    fn stick(r: &Rig, label: Option<&str>, mount: Option<&Path>, locked: bool) -> Volume {
        let _ = r;
        Volume { block: format!("/b/{}", label.unwrap_or("none")), drive: "/d/stick".into(), label: label.map(String::from), size: 8_000_000_000, mount: mount.map(Path::to_path_buf), locked, unlocked: false, removable: true, identity: gannet_core::disks::Identity { id: "stick".into(), size: 1, media_detected: 1 } }
    }

    fn drive_rig(tag: &str) -> Rig {
        let mut r = Rig::new(tag);
        let home = r.root.join("home");
        fs::create_dir_all(&home).unwrap();
        r.app.standard_places = vec![Place { label: "Home".into(), path: home, kind: PlaceKind::Home, volume: None }];
        r
    }

    #[test]
    fn drives_show_in_the_sidebar_and_one_taken_out_sends_the_window_home() {
        let mut r = drive_rig("drives-sidebar");
        let mount = r.root.join("media/PHOTOS");
        fs::create_dir_all(mount.join("inner")).unwrap();
        let photos = stick(&r, Some("PHOTOS"), Some(&mount), false);
        let unnamed = stick(&r, None, None, false);
        r.app.on_msg(Msg::Volumes(vec![photos.clone(), unnamed]));
        // Mounts udisks2 doesn't know about, like the sandbox's own, follow these.
        let labels: Vec<&str> = r.app.places.iter().take(3).map(|p| p.label.as_str()).collect();
        assert_eq!(labels, ["Home", "PHOTOS", "8.0 GB Volume"]);
        assert!(r.app.eject_rect(1).is_some() && r.app.eject_rect(0).is_none(), "drives that come out have an eject button");
        r.app.navigate(mount.join("inner"), None);
        r.pump_until("on the drive", |a| !a.loading);
        // It's still there after other changes.
        r.app.on_msg(Msg::Volumes(vec![photos.clone()]));
        assert_eq!(r.app.dir, mount.join("inner"));
        r.app.on_msg(Msg::Volumes(vec![]));
        assert_eq!(r.app.dir, r.root.join("home"), "gone, so home");
        assert!(r.app.places.iter().all(|p| p.volume.is_none()), "and out of the sidebar");
    }

    #[test]
    fn each_sidebar_section_has_a_heading_and_headings_arent_places() {
        let mut r = drive_rig("drives-heading");
        assert_eq!(r.app.sidebar_headings(), [], "no places, no headings");
        r.app.on_msg(Msg::Volumes(vec![]));
        assert_eq!(r.app.sidebar_headings().first(), Some(&(0, "Places")), "Places heads the top");
        let mount = r.root.join("media/PHOTOS");
        fs::create_dir_all(&mount).unwrap();
        r.app.on_msg(Msg::Volumes(vec![stick(&r, Some("PHOTOS"), Some(&mount), false)]));
        assert_eq!(r.app.sidebar_headings(), [(0, "Places"), (1, "Devices")], "above the first drive");
        let (home, photos) = (r.app.place_top(0), r.app.place_top(1));
        let (row, heading, gap) = (r.app.px(30.0), r.app.px(SIDEBAR_HEADING), r.app.px(SIDEBAR_GAP));
        assert_eq!(home, r.app.layout().sidebar.y + r.app.px(8.0) + heading, "Home is below the Places heading");
        assert_eq!(photos, home + row + gap + heading, "the drive's row is below a gap and its heading");
        let x = r.app.layout().sidebar.x + r.app.px(40.0);
        assert_eq!(r.app.hit(x, home - heading / 2), Hit::None, "the Places heading isn't a place");
        assert_eq!(r.app.hit(x, home + row / 2), Hit::Place(0));
        assert_eq!(r.app.hit(x, photos - heading / 2), Hit::None, "nor is the Devices heading");
        assert_eq!(r.app.hit(x, photos + row / 2), Hit::Place(1));
        let e = r.app.eject_rect(1).unwrap();
        assert_eq!((e.y, r.app.hit(e.x + e.w / 2, e.y + e.h / 2)), (photos, Hit::Eject(1)), "the eject button is on the drive's row");
        r.app.on_msg(Msg::Volumes(vec![]));
        assert!(r.app.sidebar_headings().iter().all(|&(i, _)| r.app.places[i].volume.is_none()), "only mounts udisks2 doesn't know of are left under Devices, if any");
    }

    #[test]
    fn the_eject_button_is_drawn() {
        let mut r = drive_rig("drives-paint");
        let mount = r.root.join("media/PHOTOS");
        fs::create_dir_all(&mount).unwrap();
        r.app.on_msg(Msg::Volumes(vec![stick(&r, Some("PHOTOS"), Some(&mount), false)]));
        let e = r.app.eject_rect(1).expect("a drive that comes out has one");
        let (w, h) = (r.app.w, r.app.h);
        let mut pixels = vec![0u32; (w * h) as usize];
        let mut canvas = crate::canvas::Canvas::new(&mut pixels, w, h);
        canvas.set_clip(Rect::new(0, 0, w, h));
        r.app.paint(&mut canvas);
        let surface = r.app.colors.surface;
        let inked = (e.y..e.bottom()).flat_map(|y| (e.x..e.right()).map(move |x| (x, y))).filter(|&(x, y)| pixels[(y * w + x) as usize] != surface).count();
        assert!(inked > 10, "something drawn in the button: {inked} pixels");
    }

    #[test]
    fn a_locked_drive_asks_for_its_passphrase_and_says_when_its_wrong() {
        let mut r = drive_rig("drives-unlock");
        let secret = stick(&r, Some("SECRETS"), None, true);
        r.app.on_msg(Msg::Volumes(vec![secret.clone()]));
        r.app.open_place(1);
        assert!(matches!(r.app.dialog, Some(Dialog::Unlock { .. })), "asked for the passphrase");
        for c in ["h", "u", "n", "t", "e", "r", "x"] {
            r.key(0, c);
        }
        r.key(k::KEY_BackSpace, "");
        let typed = |r: &Rig| match &r.app.dialog {
            Some(Dialog::Unlock { field, waiting, error, .. }) => (field.text.clone(), *waiting, error.clone()),
            _ => panic!("no dialog"),
        };
        assert_eq!(typed(&r).0, "hunter");
        // Somewhere for the work to go, which fails at once, as there's no bus there.
        r.app.system_bus = Some("unix:path=/nonexistent".into());
        r.key(k::KEY_Return, "");
        assert!(typed(&r).1, "waiting for udisks2");
        r.key(0, "z");
        assert_eq!(typed(&r).0, "hunter", "typing waits too");
        // The answer: it was wrong.
        r.app.on_msg(Msg::DiskDone { volume: secret.clone(), doing: Doing::Unlock(String::new()), result: Err(DiskError::WrongPassphrase) });
        let (text, waiting, error) = typed(&r);
        assert!(text.is_empty() && !waiting && error.is_some(), "cleared, to try again: {error:?}");
        // Typed again, and this time it works, and the window goes there.
        r.app.drives_busy.clear();
        r.key(0, "s");
        r.key(k::KEY_Return, "");
        assert!(typed(&r).1, "trying again");
        let inside = r.root.join("media/SECRETS");
        fs::create_dir_all(&inside).unwrap();
        r.app.on_msg(Msg::DiskDone { volume: secret, doing: Doing::Unlock(String::new()), result: Ok(Some(inside.clone())) });
        assert!(r.app.dialog.is_none());
        assert_eq!(r.app.dir, inside);
    }

    #[test]
    fn a_copys_question_waiting_behind_the_passphrase_is_asked_after_it() {
        let mut r = drive_rig("drives-conflict");
        let secret = stick(&r, Some("SECRETS"), None, true);
        r.app.on_msg(Msg::Volumes(vec![secret.clone()]));
        r.app.open_place(1);
        let (reply, _answers) = std::sync::mpsc::sync_channel(1);
        r.app.pending_conflicts.push(gannet_core::ops::Conflict { job: 1, source: "/a".into(), target: "/b".into(), both_folders: false, can_replace: true, reply });
        r.app.system_bus = Some("unix:path=/nonexistent".into());
        r.app.on_msg(Msg::DiskDone { volume: secret, doing: Doing::Unlock(String::new()), result: Err(DiskError::NoService) });
        assert!(matches!(r.app.dialog, Some(Dialog::Conflict { .. })), "the copy's question is next, or the copy would wait for ever");
    }

    #[test]
    fn a_drive_opened_after_the_user_went_elsewhere_doesnt_pull_the_window_back() {
        let mut r = drive_rig("drives-moved-on");
        let mount = r.root.join("media/PHOTOS");
        fs::create_dir_all(&mount).unwrap();
        let photos = stick(&r, Some("PHOTOS"), None, false);
        r.app.on_msg(Msg::Volumes(vec![photos.clone()]));
        r.app.system_bus = Some("unix:path=/nonexistent".into());
        r.app.open_place(1);
        assert!(r.app.drives_busy.contains(&photos.block), "mounting");
        // Meanwhile, somewhere else.
        let elsewhere = r.root.join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        r.app.navigate(elsewhere.clone(), None);
        r.app.on_msg(Msg::DiskDone { volume: photos.clone(), doing: Doing::Mount, result: Ok(Some(mount.clone())) });
        assert_eq!(r.app.dir, elsewhere, "stays where the user went");
        // Opened and left alone, it goes there.
        r.app.open_place(1);
        r.app.on_msg(Msg::DiskDone { volume: photos, doing: Doing::Mount, result: Ok(Some(mount.clone())) });
        assert_eq!(r.app.dir, mount);
    }

    #[test]
    fn unlock_while_something_else_is_under_way_says_so_rather_than_waiting() {
        let mut r = drive_rig("drives-busy-unlock");
        let secret = stick(&r, Some("SECRETS"), None, true);
        r.app.on_msg(Msg::Volumes(vec![secret.clone()]));
        r.app.system_bus = Some("unix:path=/nonexistent".into());
        r.app.drives_busy.push(secret.block.clone());
        r.app.open_place(1);
        r.key(0, "x");
        r.key(k::KEY_Return, "");
        match &r.app.dialog {
            Some(Dialog::Unlock { waiting, error, .. }) => assert!(!*waiting && error.is_some(), "not stuck waiting: {error:?}"),
            _ => panic!("no dialog"),
        }
        assert_eq!(format!("{:?}", Doing::Unlock("hunter2".into())), "Unlock(…)", "the passphrase never shows");
    }

    #[test]
    fn pressing_unlock_again_while_it_is_tried_changes_nothing() {
        let mut r = drive_rig("drives-press-twice");
        let secret = stick(&r, Some("SECRETS"), None, true);
        r.app.on_msg(Msg::Volumes(vec![secret.clone()]));
        r.app.system_bus = Some("unix:path=/nonexistent".into());
        r.app.open_place(1);
        r.key(0, "x");
        r.key(k::KEY_Return, "");
        r.app.press_dialog_button(0);
        match &r.app.dialog {
            Some(Dialog::Unlock { waiting, error, field, .. }) => assert!(*waiting && error.is_none() && field.text == "x", "still waiting on the first: {error:?}"),
            _ => panic!("no dialog"),
        }
    }

    #[test]
    fn the_passphrase_dialog_goes_with_its_drive_and_a_cancelled_prompt_says_nothing() {
        let mut r = drive_rig("drives-dialog-gone");
        let secret = stick(&r, Some("SECRETS"), None, true);
        r.app.on_msg(Msg::Volumes(vec![secret.clone()]));
        r.app.open_place(1);
        r.app.on_msg(Msg::Volumes(vec![]));
        assert!(r.app.dialog.is_none(), "pulled out while it asked");
        let mount = r.root.join("media/PHOTOS");
        fs::create_dir_all(&mount).unwrap();
        let photos = stick(&r, Some("PHOTOS"), Some(&mount), false);
        r.app.on_msg(Msg::DiskDone { volume: photos, doing: Doing::Eject, result: Err(DiskError::Cancelled) });
        assert!(r.app.notices.is_empty(), "the user said no to polkit, and knows: {:?}", r.app.notices);
    }

    #[test]
    fn udisks_stopping_leaves_the_window_be_and_a_drive_pulled_meanwhile_is_noticed() {
        let mut r = drive_rig("drives-udisks-gone");
        let mounted = r.root.join("media/STILL");
        fs::create_dir_all(&mounted).unwrap();
        let v = stick(&r, Some("STILL"), Some(&mounted), false);
        r.app.on_msg(Msg::Volumes(vec![v]));
        r.app.navigate(mounted.clone(), None);
        r.app.on_msg(Msg::NoUdisks);
        assert_eq!(r.app.dir, mounted, "nothing was unmounted, so it stays");
        assert!(r.app.places.iter().all(|p| p.volume.is_none()), "though udisks2's drives are gone from the sidebar");
        // Back, and the stick was pulled out meanwhile: it's gone, even if the mount table
        // hasn't caught up yet.
        r.app.on_msg(Msg::Volumes(vec![]));
        assert_eq!(r.app.dir, r.root.join("home"), "pulled out, so home");
    }

    #[test]
    fn a_drive_that_wont_go_says_why_and_the_window_stays() {
        let mut r = drive_rig("drives-busy");
        let mount = r.root.join("media/PHOTOS");
        fs::create_dir_all(&mount).unwrap();
        let photos = stick(&r, Some("PHOTOS"), Some(&mount), false);
        r.app.on_msg(Msg::Volumes(vec![photos.clone()]));
        r.app.navigate(mount.clone(), None);
        r.app.on_msg(Msg::DiskDone { volume: photos.clone(), doing: Doing::Eject, result: Err(DiskError::Busy) });
        assert_eq!(r.app.notices.last().map(String::as_str), Some("Couldn't eject “PHOTOS”: files on it are in use"));
        assert_eq!(r.app.dir, mount, "still there");
        r.app.on_msg(Msg::DiskDone { volume: photos, doing: Doing::Eject, result: Ok(None) });
        assert_eq!(r.app.dir, r.root.join("home"), "ejected, so home");
    }

    #[test]
    fn the_eject_button_ejects_on_the_release_and_not_if_the_pointer_left_it() {
        let mut r = drive_rig("drives-eject-release");
        r.app.system_bus = Some("unix:path=/nonexistent".into());
        let mount = r.root.join("media/PHOTOS");
        fs::create_dir_all(&mount).unwrap();
        let photos = stick(&r, Some("PHOTOS"), Some(&mount), false);
        r.app.on_msg(Msg::Volumes(vec![photos.clone()]));
        let e = r.app.eject_rect(1).unwrap();
        let (on, off) = ((e.x + e.w / 2) as f64, (e.y + e.h / 2) as f64);
        let ejecting = |r: &Rig| r.app.drives_busy.contains(&photos.block);
        // Pressed, then slid off before letting go: nothing.
        r.app.input(Input::Move { x: on, y: off });
        r.app.input(Input::Button { button: BTN_LEFT, pressed: true });
        assert!(!ejecting(&r), "not on the press");
        r.app.input(Input::Move { x: 40.0, y: off });
        r.app.input(Input::Button { button: BTN_LEFT, pressed: false });
        assert!(!ejecting(&r), "the pointer left it");
        // The pointer left the window while pressed: the release, wherever it comes, is nothing.
        r.app.input(Input::Move { x: on, y: off });
        r.app.input(Input::Button { button: BTN_LEFT, pressed: true });
        r.app.input(Input::Leave);
        r.app.input(Input::Button { button: BTN_LEFT, pressed: false });
        assert!(!ejecting(&r), "the pointer left the window");
        // Another drive takes that row between the press and the release: nothing.
        let card_mount = r.root.join("media/CARD");
        fs::create_dir_all(&card_mount).unwrap();
        let mut card = stick(&r, Some("CARD"), Some(&card_mount), false);
        card.drive = "/d/card".into();
        r.app.input(Input::Move { x: on, y: off });
        r.app.input(Input::Button { button: BTN_LEFT, pressed: true });
        r.app.on_msg(Msg::Volumes(vec![card.clone(), photos.clone()]));
        assert_eq!(r.app.places[1].label, "CARD", "the card is in that row now");
        r.app.input(Input::Button { button: BTN_LEFT, pressed: false });
        assert!(!ejecting(&r) && !r.app.drives_busy.contains(&card.block), "neither drive was ejected");
        // Pressed and let go on it: ejected.
        r.app.on_msg(Msg::Volumes(vec![photos.clone()]));
        r.app.input(Input::Button { button: BTN_LEFT, pressed: true });
        r.app.input(Input::Button { button: BTN_LEFT, pressed: false });
        assert!(ejecting(&r), "on the release");
    }

    #[test]
    fn ejecting_lets_go_of_every_folder_on_the_drive_first() {
        let mut r = drive_rig("drives-let-go");
        // Nothing answers, so nothing's really ejected: only what happens before is looked at.
        r.app.system_bus = Some("unix:path=/nonexistent".into());
        let (mount, other) = (r.root.join("media/PHOTOS"), r.root.join("media/CARD"));
        fs::create_dir_all(mount.join("inner")).unwrap();
        fs::create_dir_all(&other).unwrap();
        let photos = stick(&r, Some("PHOTOS"), Some(&mount), false);
        let mut card = stick(&r, Some("CARD"), Some(&other), false);
        card.drive = "/d/card".into();
        r.app.on_msg(Msg::Volumes(vec![photos.clone(), card]));
        for dir in [mount.clone(), mount.join("inner"), other.clone(), r.root.join("home"), mount.join("inner")] {
            r.app.navigate(dir, None);
            r.pump_until("read", |a| !a.loading);
        }
        r.app.previewer = Some(mount.join("inner/beach.png"));
        let held = |a: &App| a.cache.iter().map(|c| c.listing.dir.clone()).chain(a.listing.iter().map(|l| l.dir.clone())).filter(|d| d.starts_with(&mount)).collect::<Vec<_>>();
        assert!(held(&r.app).len() >= 2, "it was holding folders on the drive: {:?}", held(&r.app));
        r.app.cmds.clear();
        assert!(r.app.drive_act(photos, Doing::Eject));
        assert_eq!(r.app.dir, r.root.join("home"), "the window left the drive");
        assert_eq!(held(&r.app), Vec::<PathBuf>::new(), "and holds nothing on it");
        assert!(r.app.previewer.is_none() && r.app.cmds.iter().any(|c| matches!(c, Cmd::ClosePreview)), "the preview of a file on it closed");
        assert!(r.app.cache.iter().any(|c| c.listing.dir == other), "another drive's folder is still remembered");
        // On the card through a symlink: still the card.
        let link = r.root.join("home/card-link");
        std::os::unix::fs::symlink(&other, &link).unwrap();
        r.app.navigate(link.clone(), None);
        r.pump_until("read", |a| !a.loading);
        assert!(r.app.listing.as_ref().unwrap().held_dirs().iter().any(|d| d.starts_with(&other)), "the listing knows where it really is");
        // And while it's still being read, when where it really is isn't known yet.
        r.app.navigate(r.root.join("home"), None);
        r.pump_until("home", |a| !a.loading);
        r.app.cache.clear();
        r.app.navigate(link, None);
        assert!(r.app.loading);
        // A preview of a file anywhere goes too: it can be on the drive through a link.
        r.app.previewer = Some(r.root.join("home/elsewhere.png"));
        let card = r.app.volumes.iter().find(|v| v.drive == "/d/card").unwrap().clone();
        assert!(r.app.drive_act(card, Doing::Unmount));
        assert_eq!(r.app.dir, r.root.join("home"), "the window left the card it reached by a symlink");
        assert!(r.app.previewer.is_none(), "the preview closed");
    }

    #[test]
    fn answers_for_a_file_youve_moved_past_are_dropped() {
        let mut r = viewer_rig("stale", "a");
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

    fn menu_has_set_as_background(r: &mut Rig) -> bool {
        r.app.context_items().iter().any(|m| m.action == Action::SetBackground)
    }

    #[test]
    fn set_as_background_is_offered_for_one_picture_where_the_portal_can_do_it() {
        let mut r = Rig::new("set-background");
        fs::write(r.work().join("sunset.PNG"), "x").unwrap();
        fs::write(r.work().join("notes.txt"), "x").unwrap();
        fs::write(r.work().join("photo.CR2"), "x").unwrap();
        fs::write(r.work().join("favicon.ico"), "x").unwrap();
        fs::create_dir(r.work().join("pictures.png")).unwrap();
        std::os::unix::fs::symlink("sunset.PNG", r.work().join("linked.png")).unwrap();
        let w = r.work();
        r.open(w);
        r.select("sunset.PNG");
        assert!(!menu_has_set_as_background(&mut r), "not until the portal is known to offer it");
        r.app.session_bus = Some("unix:path=/nonexistent".into());
        r.app.on_msg(Msg::CanSetBackground(true));
        let items = r.app.context_items();
        let labels: Vec<_> = items.iter().map(|m| m.label).collect();
        let at = labels.iter().position(|&l| l == "Set as background").expect("offered for a picture");
        assert_eq!(labels[at - 1], "Preview", "right after Preview");
        r.select("linked.png");
        assert!(menu_has_set_as_background(&mut r), "a link to a picture is a picture too");
        for not_a_picture in ["notes.txt", "pictures.png", "photo.CR2", "favicon.ico"] {
            r.select(not_a_picture);
            assert!(!menu_has_set_as_background(&mut r), "{not_a_picture}");
        }
        r.app.select_all();
        assert!(!menu_has_set_as_background(&mut r), "not for several files");
        r.select("sunset.PNG");
        r.app.on_msg(Msg::CanSetBackground(false));
        assert!(!menu_has_set_as_background(&mut r), "the portal stopped offering it");
        r.app.on_msg(Msg::CanSetBackground(true));
        r.app.chooser = Some(crate::chooser::Chooser::new(crate::chooser::Spec::default(), &[], None));
        assert!(!menu_has_set_as_background(&mut r), "never in an Open or Save dialog");
        assert!(!r.app.chooser_allows(Action::SetBackground));
    }

    #[test]
    fn set_as_background_is_not_offered_in_the_trash() {
        let mut r = Rig::new("set-background-trash");
        trash_names(&mut r, &["sunset.png"]);
        open_trash(&mut r);
        r.app.session_bus = Some("unix:path=/nonexistent".into());
        r.app.on_msg(Msg::CanSetBackground(true));
        r.select("sunset.png");
        assert!(!menu_has_set_as_background(&mut r));
    }

    /// A stand-in `xdg-terminal-exec` in the rig, which writes the folder it started in and its
    /// arguments to `terminal.log`, or fails with `exit`.
    fn stand_in_terminal(r: &mut Rig, exit: i32) -> PathBuf {
        let bin = r.root.join("bin");
        fs::create_dir_all(&bin).unwrap();
        let log = r.root.join("terminal.log");
        let draft = r.root.join("terminal.draft");
        fs::write(&draft, format!("#!/bin/sh\nprintf '%s|%s\\n' \"$PWD\" \"$*\" >> '{}'\nexit {exit}\n", log.display())).unwrap();
        // Written by `install`, so no descriptor open for writing it can leak into another
        // test's child and make running it fail with "Text file busy".
        assert!(std::process::Command::new("install").arg("-m755").arg(&draft).arg(bin.join("xdg-terminal-exec")).status().unwrap().success());
        r.app.terminal = gannet_core::terminal::Terminal::find(Some(bin.as_os_str()), None);
        assert!(r.app.terminal.is_some());
        log
    }

    fn menu_has_open_in_terminal(r: &mut Rig) -> bool {
        r.app.context_items().iter().any(|m| m.action == Action::OpenTerminal)
    }

    #[test]
    fn open_in_terminal_is_offered_for_a_folder_and_the_empty_space() {
        let mut r = Rig::new("terminal-menu");
        fs::create_dir(r.work().join("src")).unwrap();
        fs::create_dir(r.work().join("docs")).unwrap();
        fs::write(r.work().join("notes.txt"), "x").unwrap();
        std::os::unix::fs::symlink("src", r.work().join("linked")).unwrap();
        let w = r.work();
        r.open(w);
        assert!(!menu_has_open_in_terminal(&mut r), "not without xdg-terminal-exec");
        stand_in_terminal(&mut r, 0);
        let labels: Vec<_> = r.app.context_items().iter().map(|m| m.label).collect();
        assert_eq!(labels.last(), Some(&"Open in terminal"), "last, on the empty space");
        for folder in ["src", "linked"] {
            r.select(folder);
            assert!(menu_has_open_in_terminal(&mut r), "{folder}");
        }
        r.select("notes.txt");
        assert!(!menu_has_open_in_terminal(&mut r), "not for a file");
        r.app.select_all();
        assert!(!menu_has_open_in_terminal(&mut r), "not for several items");
        r.app.clear_selection();
        r.app.chooser = Some(crate::chooser::Chooser::new(crate::chooser::Spec::default(), &[], None));
        assert!(!menu_has_open_in_terminal(&mut r), "never in an Open or Save dialog");
        assert!(!r.app.chooser_allows(Action::OpenTerminal));
    }

    #[test]
    fn open_in_terminal_is_not_offered_in_the_trash_or_recent() {
        let mut r = Rig::new("terminal-trash");
        stand_in_terminal(&mut r, 0);
        fs::create_dir(r.work().join("old")).unwrap();
        r.open(r.work());
        r.select("old");
        r.key(k::KEY_Delete, "");
        r.idle();
        open_trash(&mut r);
        assert!(!menu_has_open_in_terminal(&mut r), "the Trash's empty space");
        r.select("old");
        assert!(!menu_has_open_in_terminal(&mut r), "a folder in the Trash");
        let recent = r.app.recent_file.clone();
        fs::create_dir_all(recent.parent().unwrap()).unwrap();
        fs::write(&recent, "<?xml version=\"1.0\"?><xbel version=\"1.0\"></xbel>").unwrap();
        r.open(recent);
        assert!(r.app.is_recent_view());
        assert!(!menu_has_open_in_terminal(&mut r), "Recent");
        r.app.action(Action::OpenTerminal);
        std::thread::sleep(Duration::from_millis(200));
        assert!(!r.root.join("terminal.log").exists(), "nothing started from Recent");
    }

    #[test]
    fn open_in_terminal_opens_the_folder_selected_or_the_one_on_screen() {
        let mut r = Rig::new("terminal-open");
        fs::create_dir(r.work().join("my project")).unwrap();
        let w = r.work();
        r.open(w.clone());
        let log = stand_in_terminal(&mut r, 0);
        r.select("my project");
        r.app.action(Action::OpenTerminal);
        let wait = |n: usize| {
            let end = Instant::now() + Duration::from_secs(10);
            while fs::read_to_string(&log).map_or(0, |s| s.lines().count()) < n {
                assert!(Instant::now() < end, "the terminal wasn't asked for");
                std::thread::sleep(Duration::from_millis(20));
            }
        };
        wait(1);
        r.app.clear_selection();
        r.app.action(Action::OpenTerminal);
        wait(2);
        let p = w.join("my project");
        assert_eq!(fs::read_to_string(&log).unwrap(), format!("{}|--dir={}\n{}|--dir={}\n", p.display(), p.display(), w.display(), w.display()));
        std::thread::sleep(Duration::from_millis(200));
        while let Ok(m) = r.rx.try_recv() {
            r.app.on_msg(m);
        }
        assert!(r.app.notices.is_empty(), "{:?}", r.app.notices);
    }

    #[test]
    fn a_terminal_that_cant_start_says_so() {
        let mut r = Rig::new("terminal-fails");
        fs::create_dir(r.work().join("src")).unwrap();
        let w = r.work();
        r.open(w);
        stand_in_terminal(&mut r, 1);
        r.select("src");
        r.app.action(Action::OpenTerminal);
        r.pump_until("the notice", |a| !a.notices.is_empty());
        assert_eq!(r.app.notices, ["Couldn't open a terminal in “src”: it exited with code 1"]);
    }

    #[test]
    fn a_background_that_cant_be_set_says_so() {
        let mut r = Rig::new("set-background-fails");
        fs::write(r.work().join("sunset.png"), "x").unwrap();
        let w = r.work();
        r.open(w);
        r.select("sunset.png");
        // Nothing is listening there, so the portal can't be reached.
        r.app.session_bus = Some(format!("unix:path={}", r.root.join("no-bus").display()));
        r.app.on_msg(Msg::CanSetBackground(true));
        r.app.action(Action::SetBackground);
        r.pump_until("the notice", |a| !a.notices.is_empty());
        assert_eq!(r.app.notices, ["Couldn't set “sunset.png” as the background"]);
    }

    /// Packs the files named in `work/src` into `work/<archive>`, its format from its ending.
    fn pack(r: &Rig, archive: &str, names: &[&str]) {
        let ok = std::process::Command::new("bsdtar").arg("-a").arg("-cf").arg(r.work().join(archive)).arg("-C").arg(r.work().join("src")).args(names).status().unwrap().success();
        assert!(ok);
    }

    fn menu_has_extract(r: &Rig) -> bool {
        r.app.context_items().iter().any(|m| m.action == Action::Extract)
    }

    #[test]
    fn opening_an_archive_extracts_it_beside_itself_and_selects_the_folder() {
        let mut r = Rig::new("extract-open");
        fs::create_dir_all(r.work().join("src")).unwrap();
        fs::write(r.work().join("src/a.txt"), "a").unwrap();
        pack(&r, "photos.zip", &["a.txt"]);
        let w = r.work();
        r.open(w);
        r.select("photos.zip");
        r.key(k::KEY_Return, "");
        assert_eq!(r.app.jobs.len(), 1, "it runs as a job");
        r.pump_until("extracted and selected", |a| a.jobs.is_empty() && !a.loading && a.pending_select.is_empty() && a.listing.as_ref().is_some_and(|l| l.len() == 3));
        assert_eq!(r.selected_names(), ["photos"]);
        assert_eq!(fs::read(r.work().join("photos/a.txt")).unwrap(), b"a");
        assert!(r.app.notices.is_empty(), "{:?}", r.app.notices);
        // Undo sends the folder to the Trash.
        r.ctrl(k::KEY_z);
        r.idle();
        assert!(!r.work().join("photos").exists());
        assert!(r.root.join("data/Trash/files/photos/a.txt").exists());
    }

    #[test]
    fn extract_is_offered_only_when_every_item_selected_is_an_archive() {
        let mut r = Rig::new("extract-menu");
        fs::create_dir_all(r.work().join("src")).unwrap();
        fs::write(r.work().join("src/a.txt"), "a").unwrap();
        pack(&r, "one.tar.gz", &["a.txt"]);
        pack(&r, "one.zip", &["a.txt"]);
        fs::write(r.work().join("report.docx"), "x").unwrap();
        let w = r.work();
        r.open(w);
        r.select("one.tar.gz");
        let labels: Vec<_> = r.app.context_items().iter().map(|m| m.label).collect();
        let at = labels.iter().position(|&l| l == "Extract").expect("offered for an archive");
        assert_eq!(labels[at - 1], "Preview", "right after Preview");
        r.app.key(k::KEY_Down, "", Mods { shift: true, ..Mods::default() });
        assert_eq!(r.selected_names(), ["one.tar.gz", "one.zip"]);
        assert!(menu_has_extract(&r), "for several archives");
        r.app.action(Action::Extract);
        r.idle();
        assert!(r.work().join("one/a.txt").exists() && r.work().join("one (2)/a.txt").exists());
        for not_an_archive in ["report.docx", "src"] {
            r.select(not_an_archive);
            assert!(!menu_has_extract(&r), "{not_an_archive}");
        }
        r.app.select_all();
        assert!(!menu_has_extract(&r), "not with other things selected");
    }

    #[test]
    fn open_with_is_offered_for_files_archives_too_and_asks_for_them() {
        let mut r = Rig::new("open-with");
        fs::create_dir_all(r.work().join("sub")).unwrap();
        fs::write(r.work().join("a.txt"), "a").unwrap();
        fs::write(r.work().join("b.zip"), "PK").unwrap();
        let w = r.work();
        r.open(w.clone());
        let labels = |r: &Rig| r.app.context_items().iter().map(|m| m.label).collect::<Vec<_>>();
        r.select("a.txt");
        assert_eq!(labels(&r)[..2], ["Open", "Open with…"], "right after Open");
        // An archive opens in Gannet, so this is the way to another app.
        r.select("b.zip");
        assert!(labels(&r).contains(&"Open with…"));
        r.app.action(Action::OpenWith);
        assert!(matches!(r.app.cmds.last(), Some(Cmd::OpenWith(p)) if *p == [w.join("b.zip")]), "asks for the archive");
        assert!(r.app.jobs.is_empty(), "nothing extracted");
        r.app.select_all();
        assert!(!labels(&r).contains(&"Open with…"), "not with a folder selected");
        r.select("sub");
        assert!(!labels(&r).contains(&"Open with…"));
        r.app.chooser = Some(crate::chooser::Chooser::new(crate::chooser::Spec::default(), &[], None));
        r.select("a.txt");
        assert!(!labels(&r).contains(&"Open with…") && !r.app.chooser_allows(Action::OpenWith), "not in an Open dialog");
    }

    #[test]
    fn open_with_isnt_offered_in_the_trash() {
        let mut r = Rig::new("open-with-trash");
        trash_names(&mut r, &["a.txt"]);
        open_trash(&mut r);
        r.select("a.txt");
        assert!(r.app.open_with_files().is_none());
    }

    #[test]
    fn an_open_dialog_picks_an_archive_rather_than_extracting_it() {
        let mut r = Rig::new("extract-chooser");
        fs::create_dir_all(r.work().join("src")).unwrap();
        fs::write(r.work().join("src/a.txt"), "a").unwrap();
        pack(&r, "photos.zip", &["a.txt"]);
        let w = r.work();
        r.open(w);
        r.app.chooser = Some(crate::chooser::Chooser::new(crate::chooser::Spec::default(), &[], None));
        r.select("photos.zip");
        assert!(!menu_has_extract(&r));
        assert!(!r.app.chooser_allows(Action::Extract));
        r.key(k::KEY_Return, "");
        assert!(r.app.jobs.is_empty(), "nothing extracted");
        assert!(!r.work().join("photos").exists());
    }

    #[test]
    fn an_archive_in_the_trash_is_restored_before_extracting() {
        let mut r = Rig::new("extract-trash");
        trash_names(&mut r, &["photos.zip"]);
        open_trash(&mut r);
        r.select("photos.zip");
        assert!(!menu_has_extract(&r));
        r.key(k::KEY_Return, "");
        assert!(r.app.jobs.is_empty());
        assert_eq!(r.app.notices, ["Restore it from the Trash first, then extract it"]);
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
        let other = r.root.join(format!("disk/.Trash-{}/files/x", rustix::process::geteuid().as_raw()));
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
        let inner = r.root.join(format!("disk/.Trash-{}/files/d/e", rustix::process::geteuid().as_raw()));
        fs::create_dir_all(&inner).unwrap();
        r.open(inner.clone());
        let crumbs: Vec<PathBuf> = r.app.crumbs().into_iter().map(|(_, _, p)| p).collect();
        assert_eq!(crumbs, [r.app.trash_dir.clone(), inner.parent().unwrap().to_path_buf(), inner]);
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
        r.app.open_selecting(r.work(), vec![b"b".to_vec(), b".h".to_vec()]);
        r.pump_until("loaded", |a| !a.loading);
        let mut got = r.selected_names();
        got.sort();
        assert_eq!(got, [".h", "b"]);
        assert!(r.app.show_hidden);
        let l = r.app.listing.clone().unwrap();
        assert_eq!(l.name(r.app.rows[r.app.cursor]), b"b", "the cursor is on the first");
    }

    #[test]
    fn a_hidden_item_asked_for_first_gets_the_cursor() {
        let mut r = Rig::new("select-hidden-first");
        for n in ["a", ".h"] {
            fs::write(r.work().join(n), b"").unwrap();
        }
        r.app.open_selecting(r.work(), vec![b".h".to_vec()]);
        r.pump_until("loaded", |a| !a.loading);
        assert_eq!(r.selected_names(), [".h"]);
        assert!(r.app.cursor_shown);
        let l = r.app.listing.clone().unwrap();
        assert_eq!(l.name(r.app.rows[r.app.cursor]), b".h");
    }

    // ---- Pins ----

    /// A window with Home, the folders in `pins` pinned in GTK's bookmarks file beside an
    /// `sftp://` bookmark it doesn't show, and folders `d` and `e` and a file `f` showing.
    fn pin_rig(tag: &str, pins: &[&str]) -> Rig {
        let mut r = Rig::new(tag);
        let home = r.root.join("home");
        fs::create_dir_all(&home).unwrap();
        r.app.standard_places = vec![Place { label: "Home".into(), path: home, kind: PlaceKind::Home, volume: None }];
        r.app.places = r.app.standard_places.clone();
        let mut text = String::new();
        for (i, p) in pins.iter().enumerate() {
            let dir = r.root.join("pins").join(p);
            fs::create_dir_all(&dir).unwrap();
            text.push_str(&format!("{}\n", gannet_core::uri::to_file_uri(&dir)));
            if i == 0 {
                text.push_str("sftp://server/srv Server\n");
            }
        }
        fs::create_dir_all(r.root.join("config/gtk-3.0")).unwrap();
        fs::write(r.bookmarks_file(), text).unwrap();
        r.app.set_bookmarks(r.bookmarks_file());
        for d in ["d", "e"] {
            fs::create_dir_all(r.work().join(d)).unwrap();
        }
        fs::write(r.work().join("f"), b"f").unwrap();
        r.open(r.work());
        r
    }

    impl Rig {
        fn bookmarks_file(&self) -> PathBuf {
            self.root.join("config/gtk-3.0/bookmarks")
        }

        fn bookmarks(&self) -> String {
            fs::read_to_string(self.bookmarks_file()).unwrap_or_default()
        }

        /// The pins, by name, as the file has them.
        fn pinned(&self) -> Vec<String> {
            gannet_core::bookmarks::read(&self.bookmarks_file()).iter().map(|p| p.name()).collect()
        }

        fn labels(&self) -> Vec<String> {
            self.app.places.iter().map(|p| p.label.clone()).collect()
        }

        fn sidebar_x(&self) -> f64 {
            (self.app.layout().sidebar.x + self.app.px(60.0)) as f64
        }

        /// The middle of place `i`'s row.
        fn place_y(&self, i: usize) -> f64 {
            (self.app.place_top(i) + self.app.px(15.0)) as f64
        }

        fn row_named(&self, name: &str) -> usize {
            let l = self.app.listing.clone().unwrap();
            self.app.rows.iter().position(|&i| l.name(i) == name.as_bytes()).unwrap()
        }

        /// Presses on a row and carries it into the window's sidebar, at `y`.
        fn carry_to_sidebar(&mut self, name: &str, y: f64) {
            let row = self.row_named(name);
            press_on_row(self, row);
            self.app.input(Input::Move { x: 540.0, y: 500.0 });
            let x = self.sidebar_x();
            self.app.input(Input::Move { x, y });
        }

        fn release(&mut self) {
            self.app.input(Input::Button { button: BTN_LEFT, pressed: false });
        }

        fn drop_target(&self) -> Option<Hit> {
            match &self.app.file_drag {
                Some(FileDrag::Active(d)) => d.target.as_ref().map(|t| t.hit),
                _ => None,
            }
        }

        fn alt(&mut self, digit: &str) {
            self.app.key(0, digit, Mods { alt: true, ..Mods::default() });
        }
    }

    #[test]
    fn pins_follow_the_standard_places_under_their_own_heading_and_alt_numbers_follow_them() {
        let mut r = pin_rig("pins-shown", &["Projects", "Notes"]);
        assert_eq!(r.labels(), ["Home", "Projects", "Notes"], "the sftp bookmark isn't shown");
        assert_eq!(r.app.sidebar_headings(), [(0, "Places"), (1, "Pinned")]);
        let mount = r.root.join("media/PHOTOS");
        fs::create_dir_all(&mount).unwrap();
        r.app.on_msg(Msg::Volumes(vec![stick(&r, Some("PHOTOS"), Some(&mount), false)]));
        assert_eq!(&r.labels()[..4], ["Home", "Projects", "Notes", "PHOTOS"], "drives come after the pins");
        assert_eq!(&r.app.sidebar_headings()[..3], [(0, "Places"), (1, "Pinned"), (3, "Devices")]);
        r.alt("3");
        assert_eq!(r.app.dir, r.root.join("pins/Notes"));
        r.alt("4");
        assert_eq!(r.app.dir, mount);
    }

    #[test]
    fn a_pin_thats_gone_stays_in_the_sidebar_and_says_so_when_opened() {
        let mut r = pin_rig("pins-missing", &["Projects", "Gone"]);
        fs::remove_dir(r.root.join("pins/Gone")).unwrap();
        r.app.bookmarks_changed();
        assert_eq!(r.labels(), ["Home", "Projects", "Gone"]);
        r.alt("3");
        r.pump_until("tried", |a| !a.loading);
        assert!(r.app.load_error.is_some(), "it says the folder can't be read");
        r.alt("2");
        r.pump_until("loaded", |a| !a.loading);
        assert_eq!((r.app.dir.clone(), r.app.load_error.clone()), (r.root.join("pins/Projects"), None), "and the rest still work");
    }

    #[test]
    fn dropping_a_folder_between_places_pins_it_and_leaves_it_where_it_is() {
        let mut r = pin_rig("pins-drop", &[]);
        // Just below Home, where the Pinned heading would be.
        let below_home = (r.app.place_top(0) + r.app.px(30.0) + r.app.px(SIDEBAR_GAP) / 2) as f64;
        r.carry_to_sidebar("d", below_home);
        assert_eq!(r.drop_target(), Some(Hit::PinGap(0)));
        let Some(FileDrag::Active(d)) = &r.app.file_drag else { panic!("no drag") };
        assert_eq!(r.app.drag_label(d), "Pin “d”");
        assert!(r.app.pin_bar().is_some(), "a bar marks where it lands");
        r.release();
        r.idle();
        assert_eq!(r.pinned(), ["d"]);
        assert!(r.work().join("d").is_dir(), "pinning never moves the folder");
        assert_eq!(r.labels(), ["Home", "d"]);
        assert_eq!(r.app.pin_bar(), None, "the bar goes with the drag");
        // Over the middle of the pin, the folder goes into it as with any place.
        let y = r.place_y(1);
        r.carry_to_sidebar("e", y);
        assert_eq!(r.drop_target(), Some(Hit::Place(1)));
        // Near its top edge, it's pinned in front of it.
        let (x, y) = (r.sidebar_x(), (r.app.place_top(1) + 2) as f64);
        r.app.input(Input::Move { x, y });
        assert_eq!(r.drop_target(), Some(Hit::PinGap(0)));
        r.release();
        assert_eq!(r.pinned(), ["e", "d"]);
    }

    #[test]
    fn files_cant_be_pinned_and_a_folder_is_pinned_once() {
        let mut r = pin_rig("pins-only-folders", &[]);
        let gap = (r.app.place_top(0) + r.app.px(30.0) + r.app.px(SIDEBAR_GAP)) as f64;
        r.carry_to_sidebar("f", gap);
        assert_eq!(r.drop_target(), None, "a file has nowhere to go there");
        r.release();
        assert_eq!(r.pinned(), Vec::<String>::new());
        r.carry_to_sidebar("d", gap);
        r.release();
        r.carry_to_sidebar("d", gap);
        assert_eq!(r.drop_target(), None, "already pinned");
        r.release();
        assert_eq!(r.pinned(), ["d"]);
        // A file and a folder together aren't pinned either.
        r.select("e");
        let f = r.row_named("f");
        r.app.move_cursor(f, Mods { ctrl: true, ..Mods::default() });
        r.carry_to_sidebar("f", gap + 40.0);
        assert!(!matches!(r.drop_target(), Some(Hit::PinGap(_))));
    }

    #[test]
    fn unpinning_from_its_menu_leaves_the_folder_and_other_bookmarks_alone() {
        let mut r = pin_rig("pins-unpin", &["Projects", "Notes"]);
        let (x, y) = (r.sidebar_x(), r.place_y(1));
        r.app.input(Input::Move { x, y });
        r.app.input(Input::Button { button: BTN_RIGHT, pressed: true });
        let items: Vec<&str> = r.app.menu.as_ref().unwrap().items.iter().map(|i| i.label).collect();
        assert_eq!(items, ["Open", "Unpin"]);
        r.app.action(Action::Unpin);
        assert_eq!(r.pinned(), ["Notes"]);
        assert!(r.bookmarks().contains("sftp://server/srv Server"), "a line Gannet doesn't show is kept: {}", r.bookmarks());
        assert!(r.root.join("pins/Projects").is_dir());
        assert_eq!(r.labels(), ["Home", "Notes"]);
    }

    #[test]
    fn another_program_changing_the_file_shows_in_the_sidebar() {
        let mut r = pin_rig("pins-watch", &["Projects"]);
        fs::create_dir_all(r.root.join("pins/New")).unwrap();
        let more = format!("{}file://{} Named\n", r.bookmarks(), r.root.join("pins/New").display());
        gannet_core::bookmarks::save(&r.bookmarks_file(), more.as_bytes()).unwrap();
        r.app.bookmarks_changed();
        assert_eq!(r.labels(), ["Home", "Projects", "Named"]);
    }

    /// Presses pin `i` (counting from the first pin) and carries it to `y`.
    fn carry_pin(r: &mut Rig, i: usize, y: f64) {
        let (first, _) = r.app.pins_at();
        let (x, from) = (r.sidebar_x(), r.place_y(first + i));
        r.app.input(Input::Move { x, y: from });
        r.app.input(Input::Button { button: BTN_LEFT, pressed: true });
        r.app.input(Input::Move { x, y: from + 3.0 });
        assert!(r.app.pin_drag.as_ref().is_some_and(|d| !d.active), "a small wobble isn't a drag");
        r.app.input(Input::Move { x, y });
    }

    #[test]
    fn a_click_on_a_pin_opens_it_and_a_drag_puts_it_elsewhere() {
        let mut r = pin_rig("pins-reorder", &["A", "B", "C"]);
        let (x, y) = (r.sidebar_x(), r.place_y(2));
        r.app.input(Input::Move { x, y });
        r.app.input(Input::Button { button: BTN_LEFT, pressed: true });
        assert_eq!(r.app.dir, r.work(), "not on the press, which may be a drag");
        r.app.input(Input::Move { x: x + 2.0, y: y + 2.0 });
        r.release();
        assert_eq!(r.app.dir, r.root.join("pins/B"), "a click opens it");
        // A to below C.
        let below_c = r.place_y(3) + 12.0;
        carry_pin(&mut r, 0, below_c);
        assert!(r.app.carried_pin().is_some(), "it's dimmed while carried");
        assert_eq!(r.app.pin_drag.as_ref().unwrap().gap, Some(3));
        assert_eq!(r.app.pin_bar(), Some(r.app.place_top(3) + r.app.px(30.0)));
        r.release();
        assert_eq!(r.pinned(), ["B", "C", "A"]);
        assert_eq!(r.app.dir, r.root.join("pins/B"), "carrying it didn't open it");
        assert!(r.bookmarks().contains("sftp://server/srv Server"));
        // The numbers follow the new order.
        r.alt("4");
        assert_eq!(r.app.dir, r.root.join("pins/A"));
    }

    #[test]
    fn a_pin_only_moves_among_the_pins_and_never_moves_its_folder() {
        let mut r = pin_rig("pins-stay", &["A", "B"]);
        // Up over Home, which a folder dropped there would go into.
        let home = r.place_y(0);
        carry_pin(&mut r, 1, home);
        assert_eq!(r.app.pin_drag.as_ref().unwrap().gap, Some(0), "it's held to the top of the pins");
        r.release();
        assert_eq!(r.pinned(), ["B", "A"]);
        assert!(r.root.join("pins/A").is_dir() && !r.root.join("home/A").exists());
        // Out over the list, it goes to the end.
        carry_pin(&mut r, 0, 500.0);
        r.app.input(Input::Move { x: 540.0, y: 500.0 });
        r.release();
        assert_eq!(r.pinned(), ["A", "B"]);
        assert!(r.app.jobs.is_empty());
    }

    #[test]
    fn escape_puts_a_carried_pin_back() {
        let mut r = pin_rig("pins-escape", &["A", "B"]);
        let below = r.place_y(2) + 12.0;
        carry_pin(&mut r, 0, below);
        r.key(k::KEY_Escape, "");
        assert!(r.app.pin_drag.is_none() && r.app.pin_bar().is_none());
        r.release();
        assert_eq!(r.pinned(), ["A", "B"]);
        assert_eq!(r.app.dir, r.work(), "and the release doesn't open it");
    }

    #[test]
    fn pins_changed_elsewhere_during_a_drag_are_kept() {
        let mut r = pin_rig("pins-meanwhile", &["A", "B"]);
        let below = r.place_y(2) + 12.0;
        carry_pin(&mut r, 0, below);
        // Another window pins C at the end meanwhile.
        fs::create_dir_all(r.root.join("pins/C")).unwrap();
        let more = format!("{}file://{}\n", r.bookmarks(), r.root.join("pins/C").display());
        gannet_core::bookmarks::save(&r.bookmarks_file(), more.as_bytes()).unwrap();
        r.app.bookmarks_changed();
        assert_eq!(r.app.hover, Hit::None, "the carried pin keeps the pointer");
        assert_eq!(r.app.pointer_cursor, Cursor::Move);
        r.release();
        assert_eq!(r.pinned(), ["B", "A", "C"], "A lands where its bar was, and C stays");
        // The pin carried being unpinned elsewhere ends the drag, and doesn't bring it back.
        let below = r.place_y(3) + 12.0;
        carry_pin(&mut r, 0, below);
        let fewer = r.bookmarks().lines().filter(|l| !l.ends_with("/pins/B")).map(|l| format!("{l}\n")).collect::<String>();
        gannet_core::bookmarks::save(&r.bookmarks_file(), fewer.as_bytes()).unwrap();
        r.app.bookmarks_changed();
        assert!(r.app.pin_drag.is_none());
        r.release();
        assert_eq!(r.pinned(), ["A", "C"]);
    }

    #[test]
    fn a_drag_from_elsewhere_pins_only_after_it_has_really_moved() {
        // Hyprland reports the middle of the window when a drag arrives (DataDevice.cpp:713),
        // which can be anywhere. Here it says the space below Home, where a drop would pin.
        let mut r = pin_rig("pins-incoming", &[]);
        let (x, gap) = (r.sidebar_x(), (r.app.place_top(0) + r.app.px(40.0)) as f64);
        r.app.input(Input::DragEnter { picture: false, x, y: gap, files: true, can_copy: true, can_move: true, marker: None });
        assert!(r.app.pin_bar().is_none(), "nothing lights up from that first position");
        r.app.cmds.clear();
        r.app.input(Input::DragDrop);
        let work = r.work();
        assert!(r.app.cmds.iter().any(|c| matches!(c, Cmd::ReadDrop { dest, pin: None, .. } if *dest == work)), "a drop right away goes to the folder showing");
        // With a real motion there, it pins.
        r.app.input(Input::DragEnter { picture: false, x, y: gap, files: true, can_copy: true, can_move: true, marker: None });
        r.app.input(Input::DragMove { x, y: gap });
        let i = r.app.incoming.as_ref().unwrap();
        assert_eq!(r.app.incoming_label(i, i.target.as_ref().unwrap()), "Pin to the sidebar");
        assert!(r.app.pin_bar().is_some());
        r.app.cmds.clear();
        r.app.input(Input::DragDrop);
        assert!(r.app.cmds.iter().any(|c| matches!(c, Cmd::ReadDrop { pin: Some(Spot::End), .. })));
    }

    #[test]
    fn a_drop_from_elsewhere_pins_its_folders_and_answers_at_once() {
        let mut r = pin_rig("pins-dropped", &[]);
        let dropped = |r: &Rig, names: &[&str], can_copy: bool| Dropped { paths: names.iter().map(|n| r.work().join(n)).collect(), dest: PathBuf::new(), trash: false, can_copy, can_move: true, decided: None, all_local: true, saved: None, link: false, pin: Some(Spot::End) };
        // Hyprland refuses the pointer to every window until a drop is finished
        // (SeatManager.cpp:352), so the answer goes out before anything is asked of the disk,
        // which could hang.
        r.app.cmds.clear();
        r.app.on_msg(Msg::Dropped(dropped(&r, &["d", "f"], true)));
        assert_eq!(answered(&r.app), (true, false));
        assert_eq!(r.pinned(), Vec::<String>::new(), "the folders are only known a moment later");
        r.pump_until("pinned", |a| a.pins.len() == 1);
        assert_eq!(r.pinned(), ["d"]);
        assert_eq!(r.app.notices, ["Only folders can be pinned"]);
        assert!(r.work().join("d").is_dir() && r.work().join("f").is_file());
        // An app that only offers a move could delete its side on hearing the drop worked.
        r.app.cmds.clear();
        r.app.on_msg(Msg::Dropped(dropped(&r, &["e"], false)));
        assert_eq!(answered(&r.app), (false, true));
        r.pump_until("pinned", |a| a.pins.len() == 2);
        assert_eq!(r.pinned(), ["d", "e"]);
    }

    #[test]
    fn nothing_is_pinned_under_a_dialog() {
        let mut r = pin_rig("pins-modal", &[]);
        let (x, gap) = (r.sidebar_x(), (r.app.place_top(0) + r.app.px(40.0)) as f64);
        r.app.dialog = Some(Dialog::ConfirmQuit { at: 1 });
        r.app.input(Input::DragEnter { picture: false, x, y: gap, files: true, can_copy: true, can_move: true, marker: None });
        r.app.input(Input::DragMove { x, y: gap });
        assert!(r.app.incoming.as_ref().unwrap().target.is_none(), "the dialog takes no drop");
        assert!(r.app.pin_bar().is_none());
    }

    #[test]
    fn a_folder_already_in_the_sidebar_drops_on_whats_under_it() {
        let mut r = pin_rig("pins-already", &[]);
        let trash = r.root.join("data/Trash/files");
        r.app.places.push(Place { label: "Trash".into(), path: trash, kind: PlaceKind::Trash, volume: None });
        r.app.standard_places = r.app.places.clone();
        r.app.bookmarks_changed();
        // Near Trash's bottom edge a new folder is pinned, and a pinned one goes to the Trash.
        let edge = (r.app.place_top(1) + r.app.px(30.0) - 2) as f64;
        r.carry_to_sidebar("d", edge);
        assert_eq!(r.drop_target(), Some(Hit::PinGap(0)));
        r.release();
        r.carry_to_sidebar("d", edge);
        assert_eq!(r.drop_target(), Some(Hit::Place(1)), "it's already pinned");
        r.app.cancel_file_drag();
        // A standard place pinned in the file, as some desktops do for GTK's dialogs, shows once.
        let home = gannet_core::uri::to_file_uri(&r.root.join("home"));
        let text = format!("{home}\n{}", r.bookmarks());
        gannet_core::bookmarks::save(&r.bookmarks_file(), text.as_bytes()).unwrap();
        r.app.bookmarks_changed();
        assert_eq!(r.labels(), ["Home", "Trash", "d"]);
    }

    #[test]
    fn a_folder_carried_out_and_back_pins_only_after_a_motion() {
        let mut r = pin_rig("pins-back", &[]);
        let row = r.row_named("d");
        press_on_row(&mut r, row);
        r.app.input(Input::Move { x: -5.0, y: 200.0 });
        r.app.input(Input::Leave);
        let (x, gap) = (r.sidebar_x(), (r.app.place_top(0) + r.app.px(40.0)) as f64);
        r.app.input(Input::DragEnter { picture: false, x, y: gap, files: true, can_copy: true, can_move: true, marker: None });
        assert!(r.app.pin_bar().is_none());
        r.app.input(Input::DragMove { x, y: gap });
        assert_eq!(r.drop_target(), Some(Hit::PinGap(0)));
        r.app.cmds.clear();
        r.app.input(Input::DragDrop);
        assert_eq!(r.pinned(), ["d"]);
        assert_eq!(answered(&r.app), (true, false));
        assert!(r.work().join("d").is_dir());
    }

    // ---- A sidebar that scrolls ----

    /// Enough pins that the sidebar can't show them all in the test window.
    fn long_sidebar(tag: &str) -> Rig {
        let names: Vec<String> = (1..=30).map(|i| format!("P{i:02}")).collect();
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        pin_rig(tag, &names)
    }

    /// Lets the sidebar's timer run until it stops, as it does at the end it's scrolling to.
    fn tick_until_still(r: &mut Rig) {
        let mut now = Instant::now();
        for _ in 0..2000 {
            if r.app.side_scroll_at.is_none() {
                return;
            }
            now += Duration::from_millis(20);
            r.app.on_timer(now);
        }
        panic!("the sidebar never stopped scrolling");
    }

    #[test]
    fn a_sidebar_that_fits_doesnt_scroll() {
        let mut r = pin_rig("side-fits", &["A", "B"]);
        assert!(!r.app.scrollbar_visible(Pane::Sidebar));
        let side = r.app.layout().sidebar;
        let (x, y) = ((side.right() - 3) as f64, r.place_y(1));
        assert_eq!(r.app.hit(x as i32, y as i32), Hit::Place(1), "no scroll bar at its edge");
        // Over the sidebar, the wheel goes on scrolling the list, as before.
        for i in 0..200 {
            fs::write(r.work().join(format!("n{i:03}")), b"").unwrap();
        }
        r.app.action(Action::Reload);
        r.pump_until("listed", |a| a.rows.len() > 200);
        r.app.input(Input::Move { x: r.sidebar_x(), y });
        r.app.input(Input::Scroll { lines: 2.0, pixels: 0.0 });
        assert_eq!(r.app.side_scroll, 0.0);
        assert!(r.app.scroll > 0.0);
        // A drag at its bottom edge has nowhere to go.
        r.carry_to_sidebar("d", (side.bottom() - 4) as f64);
        assert!(r.app.side_scroll_at.is_none());
    }

    #[test]
    fn a_long_sidebar_scrolls_by_wheel_and_scroll_bar_and_what_is_under_the_pointer_follows() {
        let mut r = long_sidebar("side-wheel");
        let side = r.app.layout().sidebar;
        assert!(r.app.scrollbar_visible(Pane::Sidebar));
        let last = r.app.places.len() - 1;
        assert!(r.app.place_top(last) > side.bottom(), "the last pin is out of view");
        let (x, y) = (r.sidebar_x(), r.place_y(5));
        assert_eq!(r.app.hit(x as i32, y as i32), Hit::Place(5));
        r.app.input(Input::Move { x, y });
        // A touchpad's pixels, here exactly four rows.
        r.app.input(Input::Scroll { lines: 0.0, pixels: 4.0 * r.app.px(30.0) as f64 });
        assert_eq!(r.app.side_scroll, 4.0 * r.app.px(30.0) as f64);
        assert_eq!(r.app.scroll, 0.0, "the list stays put");
        assert_eq!(r.app.hit(x as i32, y as i32), Hit::Place(9), "the same spot is four places on");
        assert_eq!(r.app.hover, Hit::Place(9), "and the highlight moved with it");
        // The wheel stops at the end, with the last pin in full view.
        r.app.input(Input::Scroll { lines: 100.0, pixels: 0.0 });
        assert_eq!(r.app.side_scroll, r.app.max_scroll_in(Pane::Sidebar));
        assert!(r.app.place_top(last) + r.app.px(30.0) <= side.bottom());
        assert!(r.app.place_top(0) < side.y, "and Home above the top");
        // The scroll bar takes the right edge.
        let edge = side.right() - 3;
        assert_eq!(r.app.hit(edge, r.place_y(last) as i32), Hit::Scrollbar(Pane::Sidebar));
        // Its thumb dragged to the top takes the sidebar back there.
        let (top, h) = r.app.scrollbar_thumb(Pane::Sidebar);
        r.app.input(Input::Move { x: edge as f64, y: (side.y + top + h / 2) as f64 });
        r.app.input(Input::Button { button: BTN_LEFT, pressed: true });
        // Drifting onto the places while it scrolls doesn't light them up.
        r.app.input(Input::Move { x, y: (side.y + top + h / 2 - 100) as f64 });
        assert!(r.app.side_scroll < r.app.max_scroll_in(Pane::Sidebar), "the thumb moved");
        assert_eq!(r.app.hover, Hit::Scrollbar(Pane::Sidebar), "and keeps its hover");
        r.app.input(Input::Move { x: edge as f64, y: side.y as f64 - 200.0 });
        r.release();
        assert_eq!(r.app.side_scroll, 0.0);
        assert_eq!(r.app.dir, r.work(), "the thumb's press opened nothing");
    }

    #[test]
    fn dragging_a_folder_to_the_sidebars_bottom_edge_scrolls_it_and_pins_where_it_ends_up() {
        let mut r = long_sidebar("side-drag-pin");
        let side = r.app.layout().sidebar;
        r.carry_to_sidebar("d", (side.bottom() - 4) as f64);
        assert!(r.app.side_scroll_at.is_some(), "near the bottom edge it starts scrolling");
        tick_until_still(&mut r);
        assert_eq!(r.app.side_scroll, r.app.max_scroll_in(Pane::Sidebar), "all the way down");
        assert_eq!(r.drop_target(), Some(Hit::PinGap(30)), "below the last pin now");
        assert_eq!(r.app.pin_bar(), Some(r.app.place_top(30) + r.app.px(30.0)), "and the bar is drawn there");
        r.release();
        r.idle();
        assert_eq!(r.pinned().last().map(String::as_str), Some("d"));
        // Back up at the top edge, it scrolls up, and stops when the pointer leaves the edge.
        r.carry_to_sidebar("e", (side.y + 4) as f64);
        let at = r.app.side_scroll;
        r.app.on_timer(Instant::now() + Duration::from_millis(20));
        assert!(r.app.side_scroll < at);
        r.app.input(Input::Move { x: r.sidebar_x(), y: (side.y + side.h / 2) as f64 });
        assert!(r.app.side_scroll_at.is_none());
        let at = r.app.side_scroll;
        r.app.on_timer(Instant::now() + Duration::from_secs(1));
        assert_eq!(r.app.side_scroll, at);
        // The middle of a place scrolled there is a drop into it.
        let i = r.app.place_at(side.y + side.h / 2).unwrap();
        r.app.input(Input::Move { x: r.sidebar_x(), y: r.place_y(i) });
        assert_eq!(r.drop_target(), Some(Hit::Place(i)));
    }

    #[test]
    fn a_carried_pin_scrolls_the_sidebar_even_past_its_edge_and_lands_among_the_pins() {
        let mut r = long_sidebar("side-reorder");
        let side = r.app.layout().sidebar;
        // Out over the status bar, below the sidebar.
        carry_pin(&mut r, 0, (side.bottom() + 10) as f64);
        assert!(r.app.side_scroll_at.is_some());
        tick_until_still(&mut r);
        assert_eq!(r.app.side_scroll, r.app.max_scroll_in(Pane::Sidebar));
        assert_eq!(r.app.pin_drag.as_ref().unwrap().gap, Some(30), "after the last pin");
        r.release();
        assert_eq!(r.pinned().last().map(String::as_str), Some("P01"));
        assert_eq!(r.pinned().len(), 30);
    }

    #[test]
    fn a_drag_from_elsewhere_scrolls_the_sidebar_only_once_it_has_moved() {
        // Hyprland reports the middle of the window when a drag arrives (DataDevice.cpp:713).
        // Here that's at the sidebar's bottom edge, which mustn't start it scrolling.
        let mut r = long_sidebar("side-incoming");
        let side = r.app.layout().sidebar;
        let (x, y) = (r.sidebar_x(), (side.bottom() - 4) as f64);
        r.app.input(Input::DragEnter { picture: false, x, y, files: true, can_copy: true, can_move: true, marker: None });
        assert!(r.app.side_scroll_at.is_none());
        r.app.on_timer(Instant::now() + Duration::from_secs(1));
        assert_eq!(r.app.side_scroll, 0.0);
        // The real position comes with a motion, and the pointer then stays still while the
        // timer scrolls: no more motion arrives during a Wayland drag that doesn't move.
        r.app.input(Input::DragMove { x, y });
        r.app.cmds.clear();
        tick_until_still(&mut r);
        assert_eq!(r.app.side_scroll, r.app.max_scroll_in(Pane::Sidebar));
        assert_eq!(r.app.hover, Hit::None, "no place lights up as it passes under the pointer");
        assert!(!r.app.cmds.iter().any(|c| matches!(c, Cmd::Cursor(_))), "and the compositor's cursor is left alone");
        let i = r.app.incoming.as_ref().unwrap();
        assert_eq!(i.target.as_ref().map(|t| t.hit), Some(Hit::PinGap(30)), "the target followed the scroll");
        r.app.cmds.clear();
        r.app.input(Input::DragDrop);
        assert!(r.app.cmds.iter().any(|c| matches!(c, Cmd::ReadDrop { pin: Some(Spot::End), .. })));
        // Leaving stops it.
        r.app.input(Input::DragEnter { picture: false, x, y: (side.y + 4) as f64, files: true, can_copy: true, can_move: true, marker: None });
        r.app.input(Input::DragMove { x, y: (side.y + 4) as f64 });
        assert!(r.app.side_scroll_at.is_some());
        r.app.input(Input::DragLeave);
        assert!(r.app.side_scroll_at.is_none());
    }

    #[test]
    fn alt_numbers_scroll_a_place_out_of_view_into_it() {
        let mut r = long_sidebar("side-alt");
        let side = r.app.layout().sidebar;
        r.app.input(Input::Move { x: r.sidebar_x(), y: r.place_y(3) });
        r.app.input(Input::Scroll { lines: 100.0, pixels: 0.0 });
        assert!(r.app.place_top(1) < side.y);
        // Alt+2 is the first pin, which starts its section: its heading shows too.
        r.alt("2");
        assert_eq!(r.app.dir, r.root.join("pins/P01"));
        assert!(r.app.place_top(1) - r.app.px(SIDEBAR_HEADING) >= side.y, "the pin and its heading are in view");
        // One in view already doesn't move it.
        let at = r.app.side_scroll;
        r.alt("4");
        assert_eq!(r.app.side_scroll, at);
    }

    #[test]
    fn fewer_places_or_a_taller_window_never_leave_the_sidebar_scrolled_past_its_end() {
        let mut r = long_sidebar("side-clamp");
        r.app.input(Input::Move { x: r.sidebar_x(), y: r.place_y(3) });
        r.app.input(Input::Scroll { lines: 100.0, pixels: 0.0 });
        assert!(r.app.side_scroll > 0.0);
        r.app.resized(1280, 2000, 1.0);
        assert_eq!(r.app.side_scroll, 0.0, "it all fits now");
        r.app.resized(1280, 800, 1.0);
        r.app.input(Input::Scroll { lines: 100.0, pixels: 0.0 });
        let keep: String = r.bookmarks().lines().take(5).map(|l| format!("{l}\n")).collect();
        gannet_core::bookmarks::save(&r.bookmarks_file(), keep.as_bytes()).unwrap();
        r.app.bookmarks_changed();
        assert_eq!(r.app.side_scroll, 0.0);
        assert!(!r.app.scrollbar_visible(Pane::Sidebar));
    }

    /// What a screen reader would be handed now, as a server that just started gets it.
    fn a11y_snapshot(app: &mut App) -> crate::a11y::Snapshot {
        let mut link = crate::a11y::Link::new();
        let (tx, rx) = channel();
        assert!(link.shared.connect(link.shared.next_epoch(), tx));
        link.update(app);
        match rx.try_recv() {
            Ok(crate::a11y::ToServer::Snapshot(s)) => *s,
            _ => panic!("no snapshot"),
        }
    }

    fn paint_once(app: &mut App) {
        let mut px = vec![0u32; (app.w * app.h) as usize];
        app.paint(&mut crate::canvas::Canvas::new(&mut px, app.w, app.h));
    }

    #[test]
    fn a_screen_reader_reads_the_list_as_a_table_and_sorts_by_its_headings() {
        use crate::a11y::{Act, Column, Role, COLUMN_LOCATION, COLUMN_MODIFIED, COLUMN_NAME, COLUMN_SIZE, FRAME, LIST};
        let mut r = Rig::new("a11y-columns");
        fs::write(r.work().join("a.txt"), "aaaa").unwrap();
        fs::create_dir(r.work().join("sub")).unwrap();
        r.open(r.work());
        r.pump_until("sizes known", |a| a.listing.as_ref().is_some_and(|l| a.rows.iter().all(|&e| matches!(l.stat(e), Stat::Known { .. }))));
        let s = a11y_snapshot(&mut r.app);
        assert_eq!(s.get(LIST).map(|n| n.role), Some(Role::Table));
        assert_eq!(s.rows.columns.iter().map(|c| c.name).collect::<Vec<_>>(), ["Name", "Size", "Modified"]);
        let frame = &s.get(FRAME).unwrap().children;
        assert!([COLUMN_NAME, COLUMN_SIZE, COLUMN_MODIFIED].iter().all(|h| frame.contains(h)) && !frame.contains(&COLUMN_LOCATION), "{frame:?}");
        let size = s.get(COLUMN_SIZE).unwrap();
        assert_eq!((size.role, size.action.map(|a| a.keys)), (Role::ColumnHeader, Some("<Control>2")));
        assert_eq!(s.rows.sort, (Column::Name, false));
        // Each row's cells read as the list shows them: a folder has no size.
        let now = gannet_core::time::now();
        let (sub, a) = (s.rows.entry(0).unwrap(), s.rows.entry(1).unwrap());
        assert_eq!((s.rows.name(a), s.rows.cell(a, Column::Size, now)), ("a.txt".to_string(), "4 B".to_string()));
        assert_eq!(s.rows.cell(sub, Column::Size, now), "");
        let date = s.rows.cell(a, Column::Modified, now);
        assert!(!date.is_empty());
        assert_eq!(s.rows.summary(a, now), format!("4 B, {date}"), "the row says its other cells");
        assert_eq!(s.rows.summary(sub, now), date);
        // A heading sorts by its column, as a click does, and the new order is news.
        r.app.on_msg(Msg::A11y(Act::Press(COLUMN_SIZE)));
        r.pump_until("sorted by size", |a| a.sort.key == SortKey::Size && !a.rows.is_empty());
        let t = a11y_snapshot(&mut r.app);
        assert_eq!(t.rows.sort, (Column::Size, SortKey::Size.first_order().descending));
        let said = if t.rows.sort.1 { "Sorted by size, descending" } else { "Sorted by size, ascending" };
        assert_eq!(t.news_since(&s), [(said.to_string(), false)]);
        // Where search results are has a column of its own, which nothing sorts by.
        r.search("a.txt");
        let s = a11y_snapshot(&mut r.app);
        assert_eq!(s.rows.columns.iter().map(|c| c.name).collect::<Vec<_>>(), ["Name", "Location", "Size", "Modified"]);
        assert!(s.get(COLUMN_LOCATION).is_some_and(|n| n.action.is_none()));
        let e = s.rows.entry(0).unwrap();
        assert_eq!(s.rows.cell(e, Column::Location, now), "work");
    }

    #[test]
    fn a_screen_reader_goes_up_the_path_bar() {
        use crate::a11y::{Act, Role, PATH};
        let mut r = Rig::new("a11y-path");
        fs::create_dir_all(r.work().join("x/y")).unwrap();
        r.open(r.work().join("x/y"));
        paint_once(&mut r.app);
        let s = a11y_snapshot(&mut r.app);
        let links: Vec<_> = s.get(PATH).unwrap().children.iter().map(|&c| s.get(c).unwrap()).collect();
        assert!(links.iter().all(|n| n.role == Role::Link && n.parent == PATH));
        let names: Vec<&str> = links.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names[names.len() - 3..], ["work", "x", "y"]);
        assert!(links.last().is_some_and(|n| n.selected && n.showing), "the folder shown is the current part, and in view");
        assert!(links.iter().filter(|n| n.showing).all(|n| !n.rect.is_empty()), "placed as drawn");
        let x = links.len() - 2;
        // A request about a folder the window has left does nothing.
        r.app.on_msg(Msg::A11y(Act::OpenCrumb(Arc::new(r.work()), x)));
        assert_eq!(r.app.dir, r.work().join("x/y"));
        r.app.on_msg(Msg::A11y(Act::OpenCrumb(Arc::new(r.work().join("x/y")), x)));
        r.pump_until("up to x", |a| !a.loading && a.dir.ends_with("x"));
        assert_eq!(r.row_names(), ["y"]);
        assert!(r.app.cursor_shown && r.app.cursor == 0, "the cursor on the folder come from");
        // While the path is typed, the links aren't showing.
        r.app.start_path_edit();
        let s = a11y_snapshot(&mut r.app);
        assert!(!s.get(PATH).unwrap().showing);
    }

    #[test]
    fn a_screen_reader_hears_the_sidebars_headings_and_ejects_a_drive_by_name() {
        use crate::a11y::{Act, Role, EJECT0, HEADING_DEVICES, HEADING_PLACES, PLACE0, SIDEBAR};
        let mut r = drive_rig("a11y-eject");
        let mount = r.root.join("media/PHOTOS");
        fs::create_dir_all(&mount).unwrap();
        r.app.on_msg(Msg::Volumes(vec![stick(&r, Some("PHOTOS"), Some(&mount), false)]));
        let s = a11y_snapshot(&mut r.app);
        let side = &s.get(SIDEBAR).unwrap().children;
        assert_eq!(side[..4], [HEADING_PLACES, PLACE0, HEADING_DEVICES, PLACE0 + 1], "each heading before its section's first place");
        let devices = s.get(HEADING_DEVICES).unwrap();
        assert_eq!((devices.role, devices.name.as_str()), (Role::Heading, "Devices"));
        assert!(s.get(PLACE0).unwrap().children.is_empty(), "Home has no eject button");
        assert_eq!(s.get(PLACE0 + 1).unwrap().children, [EJECT0 + 1]);
        let eject = s.get(EJECT0 + 1).unwrap();
        assert_eq!((eject.role, eject.name.as_str(), eject.parent, eject.enabled), (Role::PushButton, "Eject PHOTOS", PLACE0 + 1, true));
        assert_eq!(Some(eject.rect), r.app.eject_rect(1));
        // A drive that took the place of the one asked about isn't ejected.
        r.app.on_msg(Msg::A11y(Act::Eject(1, "OTHER".into())));
        assert!(r.app.notices.is_empty());
        r.app.on_msg(Msg::A11y(Act::Eject(1, "PHOTOS".into())));
        assert!(r.app.notices.iter().any(|n| n.starts_with("Couldn't eject “PHOTOS”")), "the drive was asked to eject: {:?}", r.app.notices);
    }

    #[test]
    fn notices_and_jobs_are_news_to_a_screen_reader() {
        use crate::a11y::{Act, Role, DISMISS0, JOB0, LIST, NOTICE0, NOTICES, STOP0};
        let mut r = Rig::new("a11y-news");
        r.open(r.work());
        let s = a11y_snapshot(&mut r.app);
        assert_eq!(s.get(LIST).unwrap().description, "This folder is empty");
        // Not while a folder is being named in it.
        r.app.action(Action::NewFolder);
        assert!(matches!(r.app.edit, Edit::NewFolder(_)));
        assert_eq!(a11y_snapshot(&mut r.app).get(LIST).unwrap().description, "");
        r.key(k::KEY_Escape, "");
        // The same notice twice is two notices.
        r.app.notice("Something broke".into());
        r.app.notice("Something broke".into());
        let t = a11y_snapshot(&mut r.app);
        assert_eq!(t.news_since(&s), [("Something broke".to_string(), true), ("Something broke".to_string(), true)]);
        assert_eq!(t.get(NOTICES).unwrap().children, [NOTICE0, DISMISS0, NOTICE0 + 1, DISMISS0 + 1]);
        assert_eq!(t.get(DISMISS0 + 1).unwrap().description, "Something broke");
        // A request for a notice that has moved since does nothing.
        r.app.on_msg(Msg::A11y(Act::Dismiss(0, "Something else".into())));
        assert_eq!(r.app.notices.len(), 2);
        r.app.on_msg(Msg::A11y(Act::Dismiss(0, "Something broke".into())));
        let u = a11y_snapshot(&mut r.app);
        assert_eq!(r.app.notices.len(), 1);
        assert!(u.news_since(&t).is_empty(), "dismissing one isn't news");
        r.app.notices.clear();

        // A copy waiting on a question has lasted long enough to show, with a Stop button.
        fs::create_dir(r.work().join("dest")).unwrap();
        fs::write(r.work().join("a.txt"), "a").unwrap();
        fs::write(r.work().join("dest/a.txt"), "old").unwrap();
        r.app.start_job(Request::Copy { sources: vec![r.work().join("a.txt")], dest: r.work().join("dest") });
        r.pump_until("the question", |a| matches!(a.dialog, Some(Dialog::Conflict { .. })));
        std::thread::sleep(JOB_SHOW_AFTER);
        let s = a11y_snapshot(&mut r.app);
        let job = s.news.jobs.first().expect("the job").clone();
        assert_eq!(job.label, "Copying “a.txt” to dest");
        let bar = s.get(JOB0 + job.id).unwrap();
        assert_eq!((bar.role, bar.name.as_str(), bar.parent), (Role::ProgressBar, "Copying “a.txt” to dest", NOTICES));
        assert!(s.get(STOP0 + job.id).is_some_and(|b| b.enabled && b.description == job.label));
        // Answered, it finishes, and that's news.
        // Skip.
        r.app.press_dialog_button(1);
        r.pump_until("copied", |a| a.jobs.is_empty());
        let t = a11y_snapshot(&mut r.app);
        assert_eq!(t.news_since(&s), [("Finished: Copying “a.txt” to dest".to_string(), false)]);
        // Had it ended with errors, which post notices of their own, it wouldn't be finished.
        let mut failed = t.clone();
        failed.news.ended = vec![(job.id, false)];
        assert!(failed.news_since(&s).is_empty(), "a job with errors announced as finished");

        // A job stopped through its button posts a notice that it stopped, not that it finished.
        r.app.start_job(Request::Copy { sources: vec![r.work().join("a.txt")], dest: r.work().join("dest") });
        r.pump_until("the question again", |a| matches!(a.dialog, Some(Dialog::Conflict { .. })));
        std::thread::sleep(JOB_SHOW_AFTER);
        let s = a11y_snapshot(&mut r.app);
        let id = s.news.jobs.first().expect("the job").id;
        // Nothing behind the question can be used, so it's set aside for the button.
        r.app.on_msg(Msg::A11y(Act::Press(STOP0 + id)));
        assert!(!r.app.jobs[0].job.is_cancelled(), "stopped from behind the question");
        let question = r.app.dialog.take();
        r.app.on_msg(Msg::A11y(Act::Press(STOP0 + id)));
        r.app.dialog = question;
        let s = a11y_snapshot(&mut r.app);
        assert!(s.news.jobs.first().is_some_and(|j| j.stopping && j.progress == "stopping"));
        assert!(!s.get(STOP0 + id).unwrap().enabled, "stopping already");
        // Skip.
        r.app.press_dialog_button(1);
        r.pump_until("stopped", |a| a.jobs.is_empty());
        assert_eq!(a11y_snapshot(&mut r.app).news_since(&s), [("Stopped: Copying “a.txt” to dest".to_string(), true)]);
    }

    #[test]
    fn a_screen_reader_hears_the_preview_and_walks_it() {
        use crate::a11y::{Act, BACK, PREVIEW_CLOSE, PREVIEW_NEXT, PREVIEW_PREVIOUS};
        let mut r = viewer_rig("a11y-viewer", "a");
        let t = r.app.a11y_preview();
        assert_eq!((t.name.as_str(), t.can_step), ("a", (false, true)), "the first of three");
        r.app.on_msg(Msg::A11y(Act::Press(PREVIEW_NEXT)));
        assert_eq!(showing(&r).as_deref(), Some("b"));
        assert_eq!(r.app.a11y_preview().can_step, (true, true));
        r.app.on_msg(Msg::A11y(Act::Press(PREVIEW_PREVIOUS)));
        assert_eq!(showing(&r).as_deref(), Some("a"));
        // The preview has no Back button: a request meant for a list's does nothing here.
        r.app.cmds.clear();
        r.app.on_msg(Msg::A11y(Act::Press(BACK)));
        assert_eq!(showing(&r).as_deref(), Some("a"));
        r.app.on_msg(Msg::A11y(Act::Press(PREVIEW_CLOSE)));
        assert!(r.app.cmds.iter().any(|c| matches!(c, Cmd::Quit)), "closed");
    }

    #[test]
    fn a_screen_reader_can_use_the_save_dialogs_bar() {
        use crate::a11y::{Act, CHOOSER_CANCEL, CHOOSER_SAVE_NAME, CHOOSER_TYPE0, CHOOSER_TYPES, Role};
        let mut r = Rig::new("a11y-chooser");
        fs::write(r.work().join("a.txt"), "a").unwrap();
        fs::write(r.work().join("b.png"), "b").unwrap();
        let glob = |g: &str| vec![crate::chooser::Pattern::Glob(g.into())];
        let filters = vec![crate::chooser::Filter { label: "Text".into(), patterns: glob("*.txt") }, crate::chooser::Filter { label: "All files".into(), patterns: glob("*") }];
        let spec = crate::chooser::Spec { mode: crate::chooser::Mode::Save, name: Some("new.txt".into()), filters, ..crate::chooser::Spec::default() };
        r.app.chooser = Some(crate::chooser::Chooser::new(spec, &[], None));
        r.open(r.work());
        // The bar is placed as it was last drawn.
        let mut px = vec![0u32; (r.app.w * r.app.h) as usize];
        r.app.paint(&mut crate::canvas::Canvas::new(&mut px, r.app.w, r.app.h));
        let parts = r.app.a11y_chooser_parts();
        let name = parts.iter().find(|n| n.id == CHOOSER_SAVE_NAME).expect("the name field");
        assert_eq!((name.role, name.field.as_ref().map(|f| f.text.as_str())), (Role::Entry, Some("new.txt")));
        assert!(!name.rect.is_empty() && r.app.a11y_name_has_keyboard());
        let types = parts.iter().find(|n| n.id == CHOOSER_TYPES).expect("the file types");
        assert_eq!(types.children, [CHOOSER_TYPE0, CHOOSER_TYPE0 + 1]);
        assert!(parts.iter().any(|n| n.id == CHOOSER_TYPE0 && n.selected));
        assert!(parts.iter().filter(|n| n.parent == CHOOSER_TYPES).all(|n| n.showing && n.rect == types.rect), "the types count as showing, or a screen reader leaves them out");
        // Picking a file type shows its files.
        r.app.on_msg(Msg::A11y(Act::Focus(CHOOSER_TYPE0 + 1)));
        r.pump_until("every file", |a| a.rows.len() == 2);
        assert_eq!(r.app.chooser.as_ref().unwrap().filter, Some(1));
        // Moving to a row takes the keyboard from the name, and moving to the name gives it back.
        let dir = Arc::new(r.work());
        r.app.on_msg(Msg::A11y(Act::FocusRow(dir, b"b.png".to_vec())));
        assert!(!r.app.a11y_name_has_keyboard() && r.app.cursor_shown);
        r.app.on_msg(Msg::A11y(Act::Focus(CHOOSER_SAVE_NAME)));
        assert!(r.app.a11y_name_has_keyboard());
        // A late request about a folder the dialog has left doesn't take the keyboard from the name.
        r.app.on_msg(Msg::A11y(Act::FocusRow(Arc::new(r.work().join("gone")), b"b.png".to_vec())));
        assert!(r.app.a11y_name_has_keyboard(), "a request about another folder took the keyboard");
        r.app.on_msg(Msg::A11y(Act::Press(CHOOSER_CANCEL)));
        assert!(r.app.cmds.iter().any(|c| matches!(c, Cmd::Quit)) && r.app.chooser.as_ref().unwrap().picked.is_none(), "cancelled");
    }

    // ---- Menus and dialogs for screen readers ----

    fn snapshot(r: &mut Rig) -> crate::a11y::Snapshot {
        crate::a11y::Link::new().build(&mut r.app)
    }

    #[test]
    fn a_screen_reader_answers_a_dialog_and_cant_act_behind_it() {
        use crate::a11y::{Act, BACK, DIALOG, DIALOG_BUTTON0, FRAME, Focus as Has, LIST, PLACE0, Role};
        let mut r = Rig::new("a11y-dialog");
        fs::write(r.work().join("a.txt"), "a").unwrap();
        r.open(r.work());
        r.select("a.txt");
        r.app.key(k::KEY_Delete, "", Mods { shift: true, ..Mods::default() });
        let s = snapshot(&mut r);
        let d = s.get(DIALOG).expect("the dialog");
        assert_eq!((d.role, d.name.as_str(), d.description.as_str()), (Role::Alert, "Delete “a.txt” for good?", "This can't be undone."));
        assert_eq!(s.get(FRAME).unwrap().children.last(), Some(&DIALOG), "over the rest");
        let buttons: Vec<&str> = d.children.iter().map(|&c| s.get(c).unwrap().name.as_str()).collect();
        assert_eq!(buttons, ["Delete", "Cancel"]);
        // The keyboard is on the safe choice, which Enter presses.
        assert_eq!(s.focus, Has::Node(DIALOG_BUTTON0 + 1));
        assert_eq!(s.modal.and_then(|m| m.default), Some(DIALOG_BUTTON0 + 1));
        assert!(s.behind(LIST) && s.behind(BACK) && s.rows_behind(), "what's behind can't be used");
        assert!(!s.behind(FRAME) && !s.behind(DIALOG) && !s.behind(DIALOG_BUTTON0), "the dialog can, in its window");
        // Behind it, nothing happens.
        r.app.on_msg(Msg::A11y(Act::OpenPlace(0)));
        r.app.on_msg(Msg::A11y(Act::Press(BACK)));
        r.app.on_msg(Msg::A11y(Act::Focus(PLACE0)));
        assert!(r.app.dialog.is_some() && r.app.dir == r.work());
        // A button is pressed only if it's still the one the screen reader saw.
        r.app.on_msg(Msg::A11y(Act::Choose(DIALOG_BUTTON0, "Cancel".into())));
        assert!(r.app.dialog.is_some(), "not pressed under another's name");
        r.app.on_msg(Msg::A11y(Act::Focus(DIALOG_BUTTON0)));
        assert_eq!(snapshot(&mut r).focus, Has::Node(DIALOG_BUTTON0), "moving to a button picks it");
        r.app.on_msg(Msg::A11y(Act::Choose(DIALOG_BUTTON0 + 1, "Cancel".into())));
        assert!(r.app.dialog.is_none(), "cancelled");
        let s = snapshot(&mut r);
        assert!(s.modal.is_none() && s.get(DIALOG).is_none() && !s.behind(LIST));
        assert!(matches!(s.focus, Has::Row(_)), "the keyboard is back on the row");
        assert!(r.work().join("a.txt").exists());
    }

    #[test]
    fn a_screen_reader_reads_a_menu_and_chooses_from_it() {
        use crate::a11y::{Act, MENU, MENU_ITEM0, Focus as Has, Role};
        let mut r = Rig::new("a11y-menu");
        fs::write(r.work().join("a.txt"), "a").unwrap();
        r.open(r.work());
        r.select("a.txt");
        r.app.open_menu(100, 100);
        let s = snapshot(&mut r);
        let m = s.get(MENU).expect("the menu");
        assert_eq!(m.role, Role::Menu);
        assert_eq!(s.focus, Has::Node(MENU), "nothing picked yet, as when the pointer opens it");
        let items: Vec<&crate::a11y::Node> = m.children.iter().map(|&c| s.get(c).unwrap()).collect();
        let copy = items.iter().find(|n| n.name == "Copy").unwrap();
        assert_eq!((copy.role, copy.description.as_str(), copy.enabled), (Role::MenuItem, "Ctrl+C", true), "the shortcut is said after it");
        assert!(!items.iter().find(|n| n.name == "Undo").unwrap().enabled, "nothing to undo");
        let hidden = items.iter().position(|n| n.name == "Show hidden files").unwrap() as u64;
        // Moving to an item picks it, and a row behind the menu can't be moved to.
        r.app.on_msg(Msg::A11y(Act::Focus(MENU_ITEM0 + hidden)));
        assert_eq!(snapshot(&mut r).focus, Has::Node(MENU_ITEM0 + hidden));
        let dir = Arc::new(r.work());
        r.app.on_msg(Msg::A11y(Act::SelectRow(dir, b"a.txt".to_vec(), false)));
        assert!(r.app.menu.is_some() && r.app.sel_count == 1, "the menu stays, and the selection with it");
        r.app.on_msg(Msg::A11y(Act::Choose(MENU_ITEM0 + hidden, "Copy".into())));
        assert!(r.app.menu.is_some() && !r.app.show_hidden, "not chosen under another's name");
        r.app.on_msg(Msg::A11y(Act::Choose(MENU_ITEM0 + hidden, "Show hidden files".into())));
        assert!(r.app.menu.is_none() && r.app.show_hidden, "chosen, as clicking it does");
    }

    #[test]
    fn the_rename_dialog_has_its_fields_and_names_for_a_screen_reader() {
        use crate::a11y::{Act, DIALOG, DIALOG_BUTTON0, Focus as Has, RENAME_FIND, RENAME_NAME0, RENAME_NAMES, RENAME_REPLACE, Role};
        let mut r = Rig::new("a11y-rename-many");
        for n in ["a.jpg", "b.jpg", "keep.txt"] {
            fs::write(r.work().join(n), n).unwrap();
        }
        r.open(r.work());
        r.ctrl(k::KEY_a);
        r.key(k::KEY_F2, "");
        let s = snapshot(&mut r);
        let d = s.get(DIALOG).unwrap();
        assert_eq!((d.role, d.name.as_str()), (Role::Dialog, "Rename 3 items"));
        assert_eq!(s.focus, Has::Node(RENAME_FIND));
        assert!(!s.get(DIALOG_BUTTON0).unwrap().enabled, "nothing to rename yet");
        type_in(&mut r, ".jpg");
        r.app.on_msg(Msg::A11y(Act::Focus(RENAME_REPLACE)));
        type_in(&mut r, ".jpeg");
        let s = snapshot(&mut r);
        assert_eq!(s.focus, Has::Node(RENAME_REPLACE));
        let field = |id| s.get(id).and_then(|n| n.field.as_ref()).map(|f| f.text.as_str());
        assert_eq!((field(RENAME_FIND), field(RENAME_REPLACE)), (Some(".jpg"), Some(".jpeg")), "each field carries its text");
        assert_eq!(s.get(DIALOG).unwrap().description, "2 items will be renamed, 1 unchanged");
        let names: Vec<(&str, &str)> = s.get(RENAME_NAMES).unwrap().children.iter().map(|&c| s.get(c).unwrap()).map(|n| (n.name.as_str(), n.description.as_str())).collect();
        assert_eq!(names, [("a.jpg", "becomes “a.jpeg”"), ("b.jpg", "becomes “b.jpeg”"), ("keep.txt", "unchanged")]);
        assert!(s.get(RENAME_NAME0).unwrap().showing);
        assert!(s.get(DIALOG_BUTTON0).unwrap().enabled);
        r.app.on_msg(Msg::A11y(Act::Choose(DIALOG_BUTTON0, "Rename".into())));
        r.idle();
        assert!(r.app.dialog.is_none() && r.work().join("a.jpeg").exists());
    }

    #[test]
    fn a_clash_just_asked_cant_be_answered_by_a_screen_reader_either() {
        use crate::a11y::{Act, DIALOG_BUTTON0, DIALOG_CHECK};
        let mut r = Rig::new("a11y-clash");
        let (reply, answers) = std::sync::mpsc::sync_channel(2);
        let clash = |reply| gannet_core::ops::Conflict { job: 1, source: "/a/b.txt".into(), target: "/w/b.txt".into(), both_folders: false, can_replace: true, reply };
        r.app.show_conflict(clash(reply.clone()));
        let s = snapshot(&mut r);
        let asked = s.modal.and_then(|m| m.asked);
        assert!(asked.is_some(), "a clash says when it was asked");
        assert!(!s.get(DIALOG_BUTTON0 + 1).unwrap().enabled && !s.get(DIALOG_CHECK).unwrap().enabled, "Skip and the box wait a moment, as with the keyboard");
        assert!(r.app.next_deadline().is_some(), "and the window wakes when they can be used");
        r.app.on_msg(Msg::A11y(Act::Choose(DIALOG_BUTTON0 + 1, "Skip".into())));
        assert!(answers.try_recv().is_err() && r.app.dialog.is_some());
        r.app.dialog_guard = Some(Instant::now() - DIALOG_GUARD);
        let s = snapshot(&mut r);
        assert!(s.get(DIALOG_BUTTON0 + 1).unwrap().enabled && s.get(DIALOG_CHECK).unwrap().enabled);
        // The next clash about the same name is still a new question.
        r.app.pending_conflicts.push(clash(reply));
        r.app.on_msg(Msg::A11y(Act::Choose(DIALOG_BUTTON0 + 1, "Skip".into())));
        assert!(matches!(answers.try_recv(), Ok(Answer { choice: Choice::Skip, .. })));
        let s = snapshot(&mut r);
        assert!(s.modal.and_then(|m| m.asked).is_some_and(|t| Some(t) != asked), "asked again");
    }

    #[test]
    fn the_passphrase_is_never_given_to_a_screen_reader() {
        use crate::a11y::{DIALOG, Focus as Has, PASSPHRASE, Role};
        let mut r = drive_rig("a11y-unlock");
        let v = stick(&r, Some("SECRET"), None, true);
        let mut field = Field::new("pa");
        field.insert("ß");
        field.left(false, false);
        r.app.dialog = Some(Dialog::Unlock { volume: v, field, at: 0, error: None, waiting: false });
        let s = snapshot(&mut r);
        assert_eq!(s.get(DIALOG).map(|d| (d.role, d.name.as_str())), Some((Role::Dialog, "Unlock “SECRET”")));
        let p = s.get(PASSPHRASE).unwrap();
        let f = p.field.as_ref().unwrap();
        assert_eq!((p.role, f.text.as_str()), (Role::PasswordText, "•••"), "a dot for each character");
        assert_eq!(f.caret, "••".len(), "the caret among the dots where it is in the passphrase");
        assert_eq!(s.focus, Has::Node(PASSPHRASE), "typing goes to it");
        if let Some(Dialog::Unlock { waiting, .. }) = r.app.dialog.as_mut() {
            *waiting = true;
        }
        let s = snapshot(&mut r);
        assert!(!s.get(PASSPHRASE).unwrap().enabled && s.get(DIALOG).unwrap().description == "Unlocking…", "left alone while it's tried");
    }

    #[test]
    fn a_screen_reader_types_the_name_to_save_as() {
        use crate::a11y::{Act, CHOOSER_SAVE_NAME, Focus, Link, TextEdit};
        let mut r = Rig::new("a11y-save-name");
        fs::write(r.work().join("a.txt"), "a").unwrap();
        let spec = crate::chooser::Spec { mode: crate::chooser::Mode::Save, name: Some("new.txt".into()), ..crate::chooser::Spec::default() };
        r.app.chooser = Some(crate::chooser::Chooser::new(spec, &[], None));
        r.open(r.work());
        let mut px = vec![0u32; (r.app.w * r.app.h) as usize];
        r.app.paint(&mut crate::canvas::Canvas::new(&mut px, r.app.w, r.app.h));
        let name = |r: &Rig| r.app.chooser.as_ref().unwrap().name.text.clone();
        // The snapshot has where its characters are, and the number its requests carry.
        let s = Link::new().build(&mut r.app);
        assert_eq!(s.focus, Focus::Node(CHOOSER_SAVE_NAME));
        assert_eq!(s.geometry(CHOOSER_SAVE_NAME).map(|g| g.edges.len()), Some("new.txt".len() + 1));
        r.app.on_msg(Msg::A11y(Act::EditText(CHOOSER_SAVE_NAME, s.edit_gen, TextEdit::Replace(0, 3, "report".into()))));
        assert_eq!(name(&r), "report.txt");
        // With the keyboard in the list, typing in the name gives it the keyboard first, as a
        // click on it does.
        r.app.on_msg(Msg::A11y(Act::FocusRow(Arc::new(r.work()), b"a.txt".to_vec())));
        assert!(!r.app.a11y_name_has_keyboard());
        let edit_gen = r.app.edit_gen;
        r.app.on_msg(Msg::A11y(Act::EditText(CHOOSER_SAVE_NAME, edit_gen, TextEdit::Replace(0, usize::MAX, "b.txt".into()))));
        assert!(r.app.a11y_name_has_keyboard());
        assert_eq!(name(&r), "b.txt");
        // A request from before the filter box opened is dropped.
        r.key(k::KEY_Tab, "");
        r.key(0, "/");
        assert!(matches!(r.app.edit, Edit::Filter(_)));
        r.app.on_msg(Msg::A11y(Act::EditText(CHOOSER_SAVE_NAME, edit_gen, TextEdit::Replace(0, 0, "x".into()))));
        assert_eq!(name(&r), "b.txt");
    }
}
