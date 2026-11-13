//! The Wayland connection: one window, input, shared-memory buffers, the clipboard.
//! Painting happens only on request, paced by frame callbacks. Nothing here runs
//! unless the compositor or the app has something to say.

use std::collections::VecDeque;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rustix::fs::{MemfdFlags, ftruncate, memfd_create};
use rustix::mm::{MapFlags, ProtFlags, mmap, munmap};
use rustix::time::{Itimerspec, TimerfdClockId, TimerfdFlags, TimerfdTimerFlags, Timespec, timerfd_create, timerfd_settime};
use gannet_core::clipboard;
use wayland_client::protocol::wl_data_device_manager::DndAction;
use wayland_client::protocol::{wl_buffer, wl_callback, wl_compositor, wl_data_device, wl_data_device_manager, wl_data_offer, wl_data_source, wl_keyboard, wl_pointer, wl_registry, wl_seat, wl_shm, wl_shm_pool, wl_surface};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum, event_created_child};
use wayland_protocols::wp::cursor_shape::v1::client::{wp_cursor_shape_device_v1 as cursor_dev, wp_cursor_shape_manager_v1 as cursor_mgr};
use wayland_protocols::wp::fractional_scale::v1::client::{wp_fractional_scale_manager_v1 as frac_mgr, wp_fractional_scale_v1 as frac};
use wayland_protocols::wp::viewporter::client::{wp_viewport, wp_viewporter};
use wayland_protocols::xdg::decoration::zv1::client::{zxdg_decoration_manager_v1 as deco_mgr, zxdg_toplevel_decoration_v1 as deco};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};
use xkbcommon::xkb;

use crate::canvas::{Canvas, Rect};

/// The smallest the window can be dragged, in logical pixels: the sidebar, a usable list
/// and a few rows. A tiling layout can still make it smaller, so the app has to cope.
const MIN_SIZE: (i32, i32) = (480, 320);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mods {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub logo: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cursor {
    Default,
    Pointer,
    Text,
    Move,
    Copy,
    Alias,
    NoDrop,
}

#[derive(Debug)]
pub enum Input {
    Mods(Mods),
    /// `repeat` if this comes from the key being held rather than pressed.
    Key { keysym: u32, utf8: String, mods: Mods, repeat: bool },
    Move { x: f64, y: f64 },
    Leave,
    /// Linux button code, like BTN_LEFT.
    Button { button: u32, pressed: bool },
    /// Positive scrolls down. `lines` for a wheel, `pixels` for a touchpad, in logical units.
    Scroll { lines: f64, pixels: f64 },
    Resized,
    Close,
    Focus(bool),
    /// Another program took over the clipboard from us.
    ClipboardLost,
    /// A Wayland drag over this window, from any program, this one included.
    DragEnter { x: f64, y: f64, files: bool, can_copy: bool, can_move: bool, marker: Option<clipboard::DragMarker> },
    DragMove { x: f64, y: f64 },
    DragLeave,
    DragDrop,
    /// The Wayland drag this window started was dropped. The receiver may still be reading.
    OutDragDropped,
    /// The Wayland drag this window started is over, however it ended.
    OutDragEnded,
    /// A drop arrived while an earlier one is still being dealt with, so it was refused.
    DragDropBusy,
}

/// The picture a compositor carries with a drag, in physical pixels.
pub struct DragIcon {
    pub w: i32,
    pub h: i32,
    pub pixels: Vec<u32>,
}

/// What we offer on the clipboard, by MIME type.
pub struct Offer {
    pub mimes: Vec<(String, Arc<Vec<u8>>)>,
}

#[derive(Default)]
struct OfferData {
    mimes: Mutex<Vec<String>>,
    /// What the other program allows, as `DndAction` bits.
    source_actions: Mutex<u32>,
    /// What the compositor settled on, as `DndAction` bits.
    action: Mutex<u32>,
}

struct Buffer {
    wl: wl_buffer::WlBuffer,
    ptr: *mut u32,
    size: usize,
    w: i32,
    h: i32,
    busy: bool,
    /// Everything changed since this buffer last held the current picture.
    stale: Rect,
    retired: bool,
    id: u32,
}

impl Buffer {
    fn free(self) {
        self.wl.destroy();
        // SAFETY: ptr and size came from mmap in new_buffer, and this consumes the only owner.
        unsafe {
            let _ = munmap(self.ptr as *mut _, self.size);
        }
    }
}

pub struct State {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    wm_base: Option<xdg_wm_base::XdgWmBase>,
    seat: Option<wl_seat::WlSeat>,
    viewporter: Option<wp_viewporter::WpViewporter>,
    frac_manager: Option<frac_mgr::WpFractionalScaleManagerV1>,
    cursor_manager: Option<cursor_mgr::WpCursorShapeManagerV1>,
    data_manager: Option<wl_data_device_manager::WlDataDeviceManager>,
    deco_manager: Option<deco_mgr::ZxdgDecorationManagerV1>,

    surface: Option<wl_surface::WlSurface>,
    xdg_surface: Option<xdg_surface::XdgSurface>,
    toplevel: Option<xdg_toplevel::XdgToplevel>,
    viewport: Option<wp_viewport::WpViewport>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,
    cursor_device: Option<cursor_dev::WpCursorShapeDeviceV1>,
    data_device: Option<wl_data_device::WlDataDevice>,

    /// Logical size.
    pub width: i32,
    pub height: i32,
    pending_size: Option<(i32, i32)>,
    pub configured: bool,
    needs_commit: bool,
    /// In 120ths, as the fractional scale protocol has it.
    pub scale120: u32,
    integer_scale: i32,

    buffers: Vec<Buffer>,
    pending_damage: Rect,
    next_buffer_id: u32,
    frame_pending: bool,
    last_frame: Option<Instant>,

    pub inputs: VecDeque<Input>,

    xkb_ctx: xkb::Context,
    xkb_state: Option<xkb::State>,
    repeat_delay: i32,
    repeat_rate: i32,
    repeat_key: Option<u32>,
    repeat_timer: Option<OwnedFd>,

    pointer_serial: u32,
    /// The last input serial, which the clipboard wants as proof of a user action.
    input_serial: u32,
    axis_lines: f64,
    axis_pixels: f64,
    px: (f64, f64),

    selection: Option<wl_data_offer::WlDataOffer>,
    drag_offer: Option<wl_data_offer::WlDataOffer>,
    /// The serial of the drag's enter event, which accepting it wants.
    drag_serial: u32,
    /// Kept after a drop until its data has been read and the drop is finished.
    dropped_offer: Option<wl_data_offer::WlDataOffer>,
    /// Whether the offer being hovered, and the one dropped, were accepted. Finishing one
    /// that wasn't is a protocol error, and that would close the window.
    drag_accepting: bool,
    dropped_accepted: bool,
    /// The serial of the last button press, which starting a drag wants as proof.
    press_serial: u32,
    /// The drag this window started, and what it offers.
    dnd_source: Option<wl_data_source::WlDataSource>,
    dnd_payload: Option<Arc<Offer>>,
    /// A dropped drag's source, kept until the receiver has read it. It's held apart so that
    /// a new drag can't destroy it, and its last events can't end a new drag.
    finishing_source: Option<(wl_data_source::WlDataSource, Arc<Offer>)>,
    dnd_icon: Option<(wl_surface::WlSurface, Buffer, Option<wp_viewport::WpViewport>)>,
    source: Option<wl_data_source::WlDataSource>,
    offer: Option<Arc<Offer>>,
    cursor_now: Cursor,
}

pub struct Wl {
    pub conn: Connection,
    pub queue: EventQueue<State>,
    pub qh: QueueHandle<State>,
    pub state: State,
}

impl Wl {
    /// Connects and opens the window. `Err` has a message for the terminal.
    pub fn connect(title: &str, app_id: &str, w: i32, h: i32) -> Result<Self, String> {
        let conn = Connection::connect_to_env().map_err(|e| format!("can't connect to a Wayland compositor: {e}"))?;
        let mut queue = conn.new_event_queue();
        let qh = queue.handle();
        let _registry = conn.display().get_registry(&qh, ());
        let mut state = State {
            compositor: None,
            shm: None,
            wm_base: None,
            seat: None,
            viewporter: None,
            frac_manager: None,
            cursor_manager: None,
            data_manager: None,
            deco_manager: None,
            surface: None,
            xdg_surface: None,
            toplevel: None,
            viewport: None,
            keyboard: None,
            pointer: None,
            cursor_device: None,
            data_device: None,
            width: w,
            height: h,
            pending_size: None,
            configured: false,
            needs_commit: false,
            scale120: 120,
            integer_scale: 1,
            buffers: Vec::new(),
            pending_damage: Rect::default(),
            next_buffer_id: 0,
            frame_pending: false,
            last_frame: None,
            inputs: VecDeque::new(),
            xkb_ctx: xkb::Context::new(xkb::CONTEXT_NO_FLAGS),
            xkb_state: None,
            repeat_delay: 400,
            repeat_rate: 30,
            repeat_key: None,
            repeat_timer: timerfd_create(TimerfdClockId::Monotonic, TimerfdFlags::CLOEXEC | TimerfdFlags::NONBLOCK).ok(),
            pointer_serial: 0,
            input_serial: 0,
            axis_lines: 0.0,
            axis_pixels: 0.0,
            px: (0.0, 0.0),
            selection: None,
            drag_offer: None,
            drag_serial: 0,
            dropped_offer: None,
            drag_accepting: false,
            dropped_accepted: false,
            press_serial: 0,
            dnd_source: None,
            dnd_payload: None,
            finishing_source: None,
            dnd_icon: None,
            source: None,
            offer: None,
            cursor_now: Cursor::Default,
        };
        queue.roundtrip(&mut state).map_err(|e| format!("Wayland: {e}"))?;
        let compositor = state.compositor.clone().ok_or("the compositor lacks wl_compositor")?;
        let wm_base = state.wm_base.clone().ok_or("the compositor lacks xdg_wm_base")?;
        state.shm.as_ref().ok_or("the compositor lacks wl_shm")?;
        let surface = compositor.create_surface(&qh, ());
        let xs = wm_base.get_xdg_surface(&surface, &qh, ());
        let top = xs.get_toplevel(&qh, ());
        top.set_title(title.to_string());
        top.set_app_id(app_id.to_string());
        top.set_min_size(MIN_SIZE.0, MIN_SIZE.1);
        if let Some(m) = &state.deco_manager {
            m.get_toplevel_decoration(&top, &qh, ()).set_mode(deco::Mode::ServerSide);
        }
        if let (Some(v), Some(f)) = (&state.viewporter, &state.frac_manager) {
            state.viewport = Some(v.get_viewport(&surface, &qh, ()));
            f.get_fractional_scale(&surface, &qh, ());
        }
        surface.commit();
        state.surface = Some(surface);
        state.xdg_surface = Some(xs);
        state.toplevel = Some(top);
        conn.flush().map_err(|e| format!("Wayland: {e}"))?;
        Ok(Self { conn, queue, qh, state })
    }

    pub fn set_title(&self, title: &str) {
        if let Some(t) = &self.state.toplevel {
            t.set_title(title.to_string());
        }
    }

    pub fn fd(&self) -> BorrowedFd<'_> {
        // The connection owns the fd and outlives every use of this borrow.
        self.conn.as_fd()
    }

    pub fn repeat_fd(&self) -> Option<BorrowedFd<'_>> {
        self.state.repeat_timer.as_ref().map(|f| f.as_fd())
    }

    /// Call when the repeat timer's fd is readable.
    pub fn on_repeat_timer(&mut self) {
        let mut buf = [0u8; 8];
        if let Some(fd) = &self.state.repeat_timer {
            let _ = rustix::io::read(fd, &mut buf);
        }
        if let Some(code) = self.state.repeat_key {
            self.state.key_event(code, true);
        }
    }

    /// Physical pixels for the window.
    pub fn physical_size(&self) -> (i32, i32) {
        let s = &self.state;
        if s.viewport.is_some() {
            let scale = |v: i32| ((v as i64 * s.scale120 as i64 + 60) / 120) as i32;
            (scale(s.width), scale(s.height))
        } else {
            (s.width * s.integer_scale, s.height * s.integer_scale)
        }
    }

    /// Physical pixels per logical pixel.
    pub fn scale(&self) -> f64 {
        if self.state.viewport.is_some() { self.state.scale120 as f64 / 120.0 } else { self.state.integer_scale as f64 }
    }

    /// True if a paint now would reach the screen soon. A pending frame means wait.
    pub fn can_paint(&self) -> bool {
        self.state.configured && !self.state.frame_pending
    }

    /// True if the compositor is waiting on a commit, so we must paint regardless of pacing.
    pub fn must_paint(&self) -> bool {
        self.state.needs_commit
    }

    /// Paints one frame. `damage` is what changed since the last frame; `paint` gets
    /// a buffer and the clip it must redraw, which also covers what this buffer missed.
    pub fn paint<F: FnOnce(&mut Canvas)>(&mut self, damage: Rect, paint: F) {
        // Kept until a frame goes out, so damage isn't lost if this one can't.
        self.state.pending_damage = self.state.pending_damage.union(damage);
        let (w, h) = self.physical_size();
        if w <= 0 || h <= 0 {
            return;
        }
        let qh = self.qh.clone();
        let Some(i) = self.state.acquire_buffer(w, h, &qh) else {
            return;
        };
        let full = Rect::new(0, 0, w, h);
        let damage = std::mem::take(&mut self.state.pending_damage).intersect(full);
        for b in self.state.buffers.iter_mut() {
            b.stale = b.stale.union(damage);
        }
        let surface = self.state.surface.clone().expect("surface exists once connected");
        let b = &mut self.state.buffers[i];
        let clip = b.stale.intersect(full);
        // SAFETY: ptr maps w*h u32s for this buffer's lifetime, and the compositor doesn't
        // read it: `busy` is false, so it has released the buffer.
        let px = unsafe { std::slice::from_raw_parts_mut(b.ptr, (w * h) as usize) };
        let mut canvas = Canvas::new(px, w, h);
        canvas.set_clip(clip);
        paint(&mut canvas);
        b.stale = Rect::default();
        b.busy = true;
        surface.attach(Some(&b.wl), 0, 0);
        surface.damage_buffer(damage.x, damage.y, damage.w, damage.h);
        if let Some(v) = &self.state.viewport {
            v.set_destination(self.state.width, self.state.height);
        } else {
            surface.set_buffer_scale(self.state.integer_scale);
        }
        // A paint forced by a configure can land while a frame is pending. Two callbacks
        // would end the wait early.
        if !self.state.frame_pending {
            surface.frame(&qh, ());
        }
        self.state.frame_pending = true;
        self.state.needs_commit = false;
        surface.commit();
        self.state.trim_spare_buffers();
        self.state.last_frame = Some(Instant::now());
    }

    pub fn set_cursor(&mut self, c: Cursor) {
        if self.state.cursor_now == c {
            return;
        }
        self.state.cursor_now = c;
        if let Some(d) = &self.state.cursor_device {
            d.set_shape(self.state.pointer_serial, shape_of(c));
        }
    }

    /// Puts data on the clipboard. Needs a recent input serial, which it has after a key press.
    pub fn set_clipboard(&mut self, offer: Offer) {
        let (Some(m), Some(d)) = (&self.state.data_manager, &self.state.data_device) else { return };
        let source = m.create_data_source(&self.qh, ());
        for (mime, _) in &offer.mimes {
            source.offer(mime.clone());
        }
        d.set_selection(Some(&source), self.state.input_serial);
        if let Some(old) = self.state.source.replace(source) {
            old.destroy();
        }
        self.state.offer = Some(Arc::new(offer));
    }

    /// Empties the clipboard, if it can.
    pub fn clear_clipboard(&mut self) {
        if let Some(d) = &self.state.data_device {
            d.set_selection(None, self.state.input_serial);
        }
    }

    /// MIME types on the clipboard now.
    pub fn clipboard_mimes(&self) -> Vec<String> {
        self.state.selection.as_ref().and_then(|o| o.data::<OfferData>()).map(|d| d.mimes.lock().map(|m| m.clone()).unwrap_or_default()).unwrap_or_default()
    }

    /// Asks for clipboard contents; the read end gets them, from a thread that's the
    /// caller's to run, since the other side can stall.
    pub fn read_clipboard(&self, mime: &str) -> Option<OwnedFd> {
        let offer = self.state.selection.as_ref()?;
        let (r, w) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC).ok()?;
        offer.receive(mime.to_string(), w.as_fd());
        drop(w);
        let _ = self.conn.flush();
        Some(r)
    }
}

impl Wl {
    /// Hands a drag over to the compositor. The button has to still be down from the press.
    /// It offers one action, so whatever receives it does what the keys asked for.
    pub fn start_drag(&mut self, offer: Offer, move_only: bool, icon: Option<DragIcon>) {
        self.cancel_drag();
        let (Some(m), Some(d), Some(surface)) = (&self.state.data_manager, &self.state.data_device, &self.state.surface) else {
            self.state.inputs.push_back(Input::OutDragEnded);
            return;
        };
        let icon_surface = icon.as_ref().and_then(|_| self.state.compositor.as_ref()).map(|c| c.create_surface(&self.qh, ()));
        let source = m.create_data_source(&self.qh, ());
        for (mime, _) in &offer.mimes {
            source.offer(mime.clone());
        }
        if source.version() >= 3 {
            source.set_actions(if move_only { DndAction::Move } else { DndAction::Copy });
        }
        d.start_drag(Some(&source), surface, icon_surface.as_ref(), self.state.press_serial);
        self.state.dnd_source = Some(source);
        self.state.dnd_payload = Some(Arc::new(offer));
        if let (Some(surface), Some(icon)) = (icon_surface, icon) {
            self.show_drag_icon(surface, icon);
        }
    }

    fn show_drag_icon(&mut self, surface: wl_surface::WlSurface, icon: DragIcon) {
        let qh = self.qh.clone();
        let viewport = self.state.viewporter.as_ref().filter(|_| self.state.viewport.is_some()).map(|v| v.get_viewport(&surface, &qh, ()));
        // With a buffer scale, a buffer has to be a multiple of it, or the compositor closes
        // the connection. Whatever that adds is filled like the edge of the picture.
        let multiple = if viewport.is_some() { 1 } else { self.state.integer_scale.max(1) };
        let (bw, bh) = (round_up(icon.w, multiple), round_up(icon.h, multiple));
        let Some(buffer) = self.state.new_buffer(bw, bh, &qh) else {
            if let Some(v) = viewport {
                v.destroy();
            }
            surface.destroy();
            return;
        };
        // SAFETY: ptr maps bw*bh u32s for this buffer's lifetime, and the compositor hasn't seen it yet.
        let px = unsafe { std::slice::from_raw_parts_mut(buffer.ptr, (bw * bh) as usize) };
        copy_into_larger(&icon, px, bw);
        let scale = self.scale();
        match &viewport {
            Some(v) => v.set_destination((bw as f64 / scale).ceil() as i32, (bh as f64 / scale).ceil() as i32),
            None => surface.set_buffer_scale(multiple),
        }
        // Beside the pointer and just below it, in logical pixels. From version 5, a non-zero
        // offset in attach is a protocol error, which would close the window.
        if surface.version() >= 5 {
            surface.attach(Some(&buffer.wl), 0, 0);
            surface.offset(14, 10);
        } else {
            surface.attach(Some(&buffer.wl), 14, 10);
        }
        surface.damage_buffer(0, 0, bw, bh);
        surface.commit();
        self.state.dnd_icon = Some((surface, buffer, viewport));
    }

    /// Ends this window's drag, if the compositor still has it.
    pub fn cancel_drag(&mut self) {
        if let Some(source) = self.state.dnd_source.take() {
            source.destroy();
        }
        self.state.dnd_payload = None;
        self.state.drop_drag_icon();
    }

    /// Says whether a drop here would be taken.
    pub fn accept_drop(&mut self, accept: bool) {
        self.state.drag_accepting = accept;
        if let Some(offer) = &self.state.drag_offer {
            offer.accept(self.state.drag_serial, accept.then(|| drop_mime(offer)).flatten());
        }
    }

    /// Asks for what was dropped: its file list, and a picture if it offers one, each read
    /// from its own pipe. Both are asked for at once, as only the list says which is wanted.
    pub fn read_drop(&self) -> Option<DropRead> {
        let offer = self.state.dropped_offer.as_ref()?;
        let mimes = offer_mimes(offer);
        let ask = |mime: &str| -> Option<OwnedFd> {
            let (r, w) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC).ok()?;
            offer.receive(mime.to_string(), w.as_fd());
            Some(r)
        };
        let list = mimes.iter().any(|m| m == clipboard::URI_LIST).then(|| ask(clipboard::URI_LIST)).flatten();
        let image = gannet_core::webdrop::image_mime(&mimes).and_then(|m| Some((m, ask(m)?)));
        let _ = self.conn.flush();
        let (can_copy, can_move) = offer_allows(offer);
        (list.is_some() || image.is_some()).then_some(DropRead { list, image, can_copy, can_move })
    }

    /// Tells the other program the drop has been dealt with. If it agreed to a move, it may
    /// then delete its side, so that's only safe to say once the files have arrived.
    pub fn finish_drop(&mut self) {
        if let Some(offer) = self.state.dropped_offer.take() {
            let action = offer.data::<OfferData>().and_then(|d| d.action.lock().ok().map(|a| *a)).unwrap_or(0);
            if self.state.dropped_accepted && action != 0 && offer.version() >= 3 {
                offer.finish();
            }
            offer.destroy();
        }
    }

    /// Lets go of a drop without saying it worked, so the other program keeps its files.
    pub fn abandon_drop(&mut self) {
        if let Some(offer) = self.state.dropped_offer.take() {
            offer.destroy();
        }
    }
}

fn round_up(v: i32, multiple: i32) -> i32 {
    (v + multiple - 1) / multiple * multiple
}

/// Copies a picture into the top left of a buffer that may be larger.
fn copy_into_larger(icon: &DragIcon, px: &mut [u32], bw: i32) {
    px.fill(icon.pixels.first().copied().unwrap_or(0));
    for (row, line) in icon.pixels.chunks(icon.w.max(1) as usize).enumerate() {
        let start = row * bw as usize;
        px[start..start + line.len()].copy_from_slice(line);
    }
}

/// What this window will agree to for a drop. Move stays out unless it's all that's on offer.
fn allowed_actions(can_copy: bool) -> DndAction {
    if can_copy { DndAction::Copy } else { DndAction::Copy | DndAction::Move }
}

/// The formats an offer has.
fn offer_mimes(offer: &wl_data_offer::WlDataOffer) -> Vec<String> {
    offer.data::<OfferData>().and_then(|d| d.mimes.lock().ok().map(|m| m.clone())).unwrap_or_default()
}

/// What a drop is asked for: a file list, or failing that a picture, as a browser offers.
fn drop_mime(offer: &wl_data_offer::WlDataOffer) -> Option<String> {
    let mimes = offer_mimes(offer);
    if mimes.iter().any(|m| m == clipboard::URI_LIST) {
        return Some(clipboard::URI_LIST.to_string());
    }
    gannet_core::webdrop::image_mime(&mimes).map(str::to_string)
}

/// A dropped offer, asked for what it has: its file list, and a picture if it offers one.
pub struct DropRead {
    pub list: Option<OwnedFd>,
    pub image: Option<(&'static str, OwnedFd)>,
    pub can_copy: bool,
    pub can_move: bool,
}

fn offer_allows(offer: &wl_data_offer::WlDataOffer) -> (bool, bool) {
    let bits = offer.data::<OfferData>().and_then(|d| d.source_actions.lock().ok().map(|a| *a)).unwrap_or(0);
    (bits & DndAction::Copy.bits() != 0, bits & DndAction::Move.bits() != 0)
}

fn shape_of(c: Cursor) -> cursor_dev::Shape {
    match c {
        Cursor::Default => cursor_dev::Shape::Default,
        Cursor::Pointer => cursor_dev::Shape::Pointer,
        Cursor::Text => cursor_dev::Shape::Text,
        Cursor::Move => cursor_dev::Shape::Move,
        Cursor::Copy => cursor_dev::Shape::Copy,
        Cursor::Alias => cursor_dev::Shape::Alias,
        Cursor::NoDrop => cursor_dev::Shape::NoDrop,
    }
}

impl State {
    fn drop_drag_icon(&mut self) {
        if let Some((surface, buffer, viewport)) = self.dnd_icon.take() {
            if let Some(v) = viewport {
                v.destroy();
            }
            surface.destroy();
            buffer.free();
        }
    }

    /// The first free buffer of the right size, making one if none is free.
    fn acquire_buffer(&mut self, w: i32, h: i32, qh: &QueueHandle<State>) -> Option<usize> {
        // Wrong-size buffers can go now if free, or on release if not.
        let mut i = 0;
        while i < self.buffers.len() {
            if self.buffers[i].w != w || self.buffers[i].h != h {
                if self.buffers[i].busy {
                    self.buffers[i].retired = true;
                    i += 1;
                } else {
                    self.buffers.swap_remove(i).free();
                }
            } else {
                i += 1;
            }
        }
        if let Some(i) = self.buffers.iter().position(|b| !b.busy && !b.retired) {
            return Some(i);
        }
        let b = self.new_buffer(w, h, qh)?;
        self.buffers.push(b);
        Some(self.buffers.len() - 1)
    }

    fn new_buffer(&mut self, w: i32, h: i32, qh: &QueueHandle<State>) -> Option<Buffer> {
        let size = (w as usize) * (h as usize) * 4;
        if size > i32::MAX as usize {
            return None;
        }
        let fd = memfd_create("gannet-buffer", MemfdFlags::CLOEXEC).ok()?;
        ftruncate(&fd, size as u64).ok()?;
        // SAFETY: fd is a new memfd of `size` bytes, mapped shared and unmapped only by Buffer::free.
        let ptr = unsafe { mmap(std::ptr::null_mut(), size, ProtFlags::READ | ProtFlags::WRITE, MapFlags::SHARED, &fd, 0).ok()? } as *mut u32;
        let pool = self.shm.as_ref()?.create_pool(fd.as_fd(), size as i32, qh, ());
        let id = self.next_buffer_id;
        self.next_buffer_id = id.wrapping_add(1);
        let wl = pool.create_buffer(0, w, h, w * 4, wl_shm::Format::Xrgb8888, qh, id);
        pool.destroy();
        Some(Buffer { wl, ptr, size, w, h, busy: false, stale: Rect::new(0, 0, w, h), retired: false, id })
    }

    /// After a quiet spell, frees buffers kept only for smooth scrolling.
    fn trim_spare_buffers(&mut self) {
        let quiet = self.last_frame.is_none_or(|t| t.elapsed() > Duration::from_millis(500));
        if !quiet {
            return;
        }
        let mut i = 0;
        while i < self.buffers.len() {
            if !self.buffers[i].busy {
                self.buffers.swap_remove(i).free();
            } else {
                i += 1;
            }
        }
    }

    fn mods(&self) -> Mods {
        let Some(s) = &self.xkb_state else { return Mods::default() };
        let on = |name: &str| s.mod_name_is_active(name, xkb::STATE_MODS_EFFECTIVE);
        Mods { ctrl: on(xkb::MOD_NAME_CTRL), shift: on(xkb::MOD_NAME_SHIFT), alt: on(xkb::MOD_NAME_ALT), logo: on(xkb::MOD_NAME_LOGO) }
    }

    /// Emits a key press for a Wayland key code.
    fn key_event(&mut self, code: u32, repeat: bool) {
        let Some(s) = &self.xkb_state else { return };
        let kc = xkb::Keycode::new(code + 8);
        let keysym = s.key_get_one_sym(kc).raw();
        let utf8 = s.key_get_utf8(kc);
        let mods = self.mods();
        self.inputs.push_back(Input::Key { keysym, utf8, mods, repeat });
    }

    fn arm_repeat(&self, on: bool) {
        let Some(fd) = &self.repeat_timer else { return };
        let (first, every) = if on && self.repeat_rate > 0 { ((self.repeat_delay as i64).max(1) * 1_000_000, 1_000_000_000 / self.repeat_rate as i64) } else { (0, 0) };
        let split = |ns: i64| Timespec { tv_sec: ns / 1_000_000_000, tv_nsec: ns % 1_000_000_000 };
        let _ = timerfd_settime(fd, TimerfdTimerFlags::empty(), &Itimerspec { it_interval: split(every), it_value: split(first) });
    }

    fn apply_size(&mut self) {
        if let Some((w, h)) = self.pending_size.take()
            && w > 0
            && h > 0
            && (w != self.width || h != self.height)
        {
            self.width = w;
            self.height = h;
            self.inputs.push_back(Input::Resized);
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(s: &mut State, r: &wl_registry::WlRegistry, e: wl_registry::Event, _: &(), _: &Connection, qh: &QueueHandle<State>) {
        let wl_registry::Event::Global { name, interface, version } = e else { return };
        match interface.as_str() {
            "wl_compositor" => s.compositor = Some(r.bind(name, version.min(6), qh, ())),
            "wl_shm" => s.shm = Some(r.bind(name, 1, qh, ())),
            "xdg_wm_base" => s.wm_base = Some(r.bind(name, 1, qh, ())),
            "wl_seat" => s.seat = Some(r.bind(name, version.min(8), qh, ())),
            "wp_viewporter" => s.viewporter = Some(r.bind(name, 1, qh, ())),
            "wp_fractional_scale_manager_v1" => s.frac_manager = Some(r.bind(name, 1, qh, ())),
            "wp_cursor_shape_manager_v1" => s.cursor_manager = Some(r.bind(name, 1, qh, ())),
            "wl_data_device_manager" => {
                let m: wl_data_device_manager::WlDataDeviceManager = r.bind(name, version.min(3), qh, ());
                s.data_manager = Some(m);
            }
            "zxdg_decoration_manager_v1" => s.deco_manager = Some(r.bind(name, 1, qh, ())),
            _ => {}
        }
        if let (Some(seat), Some(m), None) = (&s.seat, &s.data_manager, &s.data_device) {
            s.data_device = Some(m.get_data_device(seat, qh, ()));
        }
    }
}

macro_rules! ignore_events {
    ($($t:ty),* $(,)?) => { $(
        impl Dispatch<$t, ()> for State {
            fn event(_: &mut State, _: &$t, _: <$t as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<State>) {}
        }
    )* };
}

ignore_events!(
    wl_compositor::WlCompositor,
    wl_shm::WlShm,
    wl_shm_pool::WlShmPool,
    wp_viewporter::WpViewporter,
    wp_viewport::WpViewport,
    frac_mgr::WpFractionalScaleManagerV1,
    cursor_mgr::WpCursorShapeManagerV1,
    cursor_dev::WpCursorShapeDeviceV1,
    deco_mgr::ZxdgDecorationManagerV1,
    deco::ZxdgToplevelDecorationV1,
    wl_data_device_manager::WlDataDeviceManager,
);

impl Dispatch<wl_surface::WlSurface, ()> for State {
    fn event(s: &mut State, surface: &wl_surface::WlSurface, e: wl_surface::Event, _: &(), _: &Connection, _: &QueueHandle<State>) {
        // A drag's picture is a surface too, but this is about the window.
        if let wl_surface::Event::PreferredBufferScale { factor } = e
            && s.surface.as_ref() == Some(surface)
            && s.viewport.is_none()
            && factor != s.integer_scale
        {
            s.integer_scale = factor;
            s.inputs.push_back(Input::Resized);
        }
    }
}

impl Dispatch<frac::WpFractionalScaleV1, ()> for State {
    fn event(s: &mut State, _: &frac::WpFractionalScaleV1, e: frac::Event, _: &(), _: &Connection, _: &QueueHandle<State>) {
        if let frac::Event::PreferredScale { scale } = e
            && scale != s.scale120
        {
            s.scale120 = scale;
            s.inputs.push_back(Input::Resized);
        }
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for State {
    fn event(_: &mut State, b: &xdg_wm_base::XdgWmBase, e: xdg_wm_base::Event, _: &(), _: &Connection, _: &QueueHandle<State>) {
        if let xdg_wm_base::Event::Ping { serial } = e {
            b.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for State {
    fn event(s: &mut State, x: &xdg_surface::XdgSurface, e: xdg_surface::Event, _: &(), _: &Connection, _: &QueueHandle<State>) {
        if let xdg_surface::Event::Configure { serial } = e {
            x.ack_configure(serial);
            s.apply_size();
            s.configured = true;
            s.needs_commit = true;
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for State {
    fn event(s: &mut State, _: &xdg_toplevel::XdgToplevel, e: xdg_toplevel::Event, _: &(), _: &Connection, _: &QueueHandle<State>) {
        match e {
            xdg_toplevel::Event::Configure { width, height, .. } => {
                if width > 0 && height > 0 {
                    s.pending_size = Some((width, height));
                }
            }
            xdg_toplevel::Event::Close => s.inputs.push_back(Input::Close),
            _ => {}
        }
    }
}

impl Dispatch<wl_callback::WlCallback, ()> for State {
    fn event(s: &mut State, _: &wl_callback::WlCallback, e: wl_callback::Event, _: &(), _: &Connection, _: &QueueHandle<State>) {
        if let wl_callback::Event::Done { .. } = e {
            s.frame_pending = false;
        }
    }
}

impl Dispatch<wl_buffer::WlBuffer, u32> for State {
    fn event(s: &mut State, _: &wl_buffer::WlBuffer, e: wl_buffer::Event, id: &u32, _: &Connection, _: &QueueHandle<State>) {
        if let wl_buffer::Event::Release = e
            && let Some(i) = s.buffers.iter().position(|b| b.id == *id)
        {
            if s.buffers[i].retired {
                s.buffers.swap_remove(i).free();
            } else {
                s.buffers[i].busy = false;
            }
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(s: &mut State, seat: &wl_seat::WlSeat, e: wl_seat::Event, _: &(), _: &Connection, qh: &QueueHandle<State>) {
        if let wl_seat::Event::Capabilities { capabilities: WEnum::Value(c) } = e {
            // Devices come and go, say with a KVM switch or an unplugged keyboard. The old
            // objects stop working when they do, so each return needs fresh ones.
            if c.contains(wl_seat::Capability::Keyboard) {
                if s.keyboard.is_none() {
                    s.keyboard = Some(seat.get_keyboard(qh, ()));
                }
            } else if let Some(k) = s.keyboard.take() {
                k.release();
                s.repeat_key = None;
                s.arm_repeat(false);
            }
            if c.contains(wl_seat::Capability::Pointer) {
                if s.pointer.is_none() {
                    let p = seat.get_pointer(qh, ());
                    if let Some(m) = &s.cursor_manager {
                        s.cursor_device = Some(m.get_pointer(&p, qh, ()));
                    }
                    s.pointer = Some(p);
                }
            } else if let Some(p) = s.pointer.take() {
                if let Some(d) = s.cursor_device.take() {
                    d.destroy();
                }
                p.release();
            }
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for State {
    fn event(s: &mut State, _: &wl_keyboard::WlKeyboard, e: wl_keyboard::Event, _: &(), _: &Connection, _: &QueueHandle<State>) {
        match e {
            wl_keyboard::Event::Keymap { format: WEnum::Value(wl_keyboard::KeymapFormat::XkbV1), fd, size } => {
                // Read by position: the fd's shared offset isn't ours to move.
                let mut bytes = vec![0u8; size as usize];
                if std::os::unix::fs::FileExt::read_exact_at(&std::fs::File::from(fd), &mut bytes, 0).is_err() {
                    return;
                }
                while bytes.last() == Some(&0) {
                    bytes.pop();
                }
                let Ok(text) = String::from_utf8(bytes) else { return };
                if let Some(map) = xkb::Keymap::new_from_string(&s.xkb_ctx, text, xkb::KEYMAP_FORMAT_TEXT_V1, xkb::KEYMAP_COMPILE_NO_FLAGS) {
                    s.xkb_state = Some(xkb::State::new(&map));
                }
            }
            wl_keyboard::Event::Enter { serial, .. } => {
                s.input_serial = serial;
                s.inputs.push_back(Input::Focus(true));
            }
            wl_keyboard::Event::Leave { .. } => {
                s.repeat_key = None;
                s.arm_repeat(false);
                s.inputs.push_back(Input::Focus(false));
            }
            wl_keyboard::Event::Modifiers { serial, mods_depressed, mods_latched, mods_locked, group } => {
                s.input_serial = serial;
                if let Some(st) = &mut s.xkb_state {
                    st.update_mask(mods_depressed, mods_latched, mods_locked, 0, 0, group);
                }
                let m = s.mods();
                s.inputs.push_back(Input::Mods(m));
            }
            wl_keyboard::Event::RepeatInfo { rate, delay } => {
                s.repeat_rate = rate;
                s.repeat_delay = delay;
            }
            wl_keyboard::Event::Key { serial, key, state: WEnum::Value(st), .. } => {
                s.input_serial = serial;
                match st {
                    wl_keyboard::KeyState::Pressed => {
                        s.key_event(key, false);
                        let repeats = s.xkb_state.as_ref().is_some_and(|x| x.get_keymap().key_repeats(xkb::Keycode::new(key + 8)));
                        if repeats {
                            s.repeat_key = Some(key);
                            s.arm_repeat(true);
                        }
                    }
                    wl_keyboard::KeyState::Released if s.repeat_key == Some(key) => {
                        s.repeat_key = None;
                        s.arm_repeat(false);
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for State {
    fn event(s: &mut State, _: &wl_pointer::WlPointer, e: wl_pointer::Event, _: &(), _: &Connection, _: &QueueHandle<State>) {
        match e {
            wl_pointer::Event::Enter { serial, surface_x, surface_y, .. } => {
                s.pointer_serial = serial;
                if let Some(d) = &s.cursor_device {
                    d.set_shape(serial, shape_of(s.cursor_now));
                }
                s.px = (surface_x, surface_y);
                s.inputs.push_back(Input::Move { x: surface_x, y: surface_y });
            }
            wl_pointer::Event::Leave { .. } => s.inputs.push_back(Input::Leave),
            wl_pointer::Event::Motion { surface_x, surface_y, .. } => {
                s.px = (surface_x, surface_y);
                s.inputs.push_back(Input::Move { x: surface_x, y: surface_y });
            }
            wl_pointer::Event::Button { serial, button, state: WEnum::Value(st), .. } => {
                s.input_serial = serial;
                if st == wl_pointer::ButtonState::Pressed {
                    s.press_serial = serial;
                }
                s.inputs.push_back(Input::Button { button, pressed: st == wl_pointer::ButtonState::Pressed });
            }
            wl_pointer::Event::Axis { axis: WEnum::Value(wl_pointer::Axis::VerticalScroll), value, .. } => s.axis_pixels += value,
            wl_pointer::Event::AxisValue120 { axis: WEnum::Value(wl_pointer::Axis::VerticalScroll), value120 } => s.axis_lines += value120 as f64 / 120.0,
            wl_pointer::Event::Frame if s.axis_lines != 0.0 || s.axis_pixels != 0.0 => {
                let (lines, pixels) = (std::mem::take(&mut s.axis_lines), std::mem::take(&mut s.axis_pixels));
                s.inputs.push_back(if lines != 0.0 { Input::Scroll { lines, pixels: 0.0 } } else { Input::Scroll { lines: 0.0, pixels } });
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_data_device::WlDataDevice, ()> for State {
    fn event(s: &mut State, _: &wl_data_device::WlDataDevice, e: wl_data_device::Event, _: &(), _: &Connection, _: &QueueHandle<State>) {
        match e {
            wl_data_device::Event::Selection { id } => {
                if let Some(old) = std::mem::replace(&mut s.selection, id) {
                    old.destroy();
                }
            }
            wl_data_device::Event::Enter { serial, x, y, id, .. } => {
                s.drag_serial = serial;
                // Files, or a picture from a browser, which is saved as a new file.
                let files = id.as_ref().is_some_and(|o| drop_mime(o).is_some());
                let marker = id.as_ref().and_then(|o| o.data::<OfferData>()).and_then(|d| d.mimes.lock().ok().and_then(|m| m.iter().find_map(|m| clipboard::DragMarker::parse(m))));
                let (can_copy, can_move) = id.as_ref().map_or((false, false), offer_allows);
                if let Some(offer) = id.as_ref().filter(|o| o.version() >= 3) {
                    // Only a copy whenever the other program allows one, so that no compositor
                    // or held key can settle on a move and have it delete its files. The drop is
                    // finished before the files have arrived, so that would lose them. If it
                    // only offers a move, it's asking for one. This window does the work either
                    // way, and another Gannet window deletes nothing on hearing this, so a window
                    // that moves its files can still agree on a copy here.
                    offer.set_actions(allowed_actions(can_copy), DndAction::Copy);
                }
                s.drag_accepting = false;
                if let Some(old) = std::mem::replace(&mut s.drag_offer, id) {
                    old.destroy();
                }
                s.inputs.push_back(Input::DragEnter { x, y, files, can_copy, can_move, marker });
            }
            wl_data_device::Event::Motion { x, y, .. } => s.inputs.push_back(Input::DragMove { x, y }),
            wl_data_device::Event::Leave => {
                if let Some(old) = s.drag_offer.take() {
                    old.destroy();
                }
                s.inputs.push_back(Input::DragLeave);
            }
            wl_data_device::Event::Drop => {
                // One at a time: an answer has to reach the drop it's about. The earlier one
                // might be a move in progress, and answering it wrongly could lose files.
                if s.dropped_offer.is_some() {
                    if let Some(offer) = s.drag_offer.take() {
                        offer.destroy();
                    }
                    s.inputs.push_back(Input::DragDropBusy);
                } else {
                    s.dropped_offer = s.drag_offer.take();
                    s.dropped_accepted = s.drag_accepting;
                    s.inputs.push_back(Input::DragDrop);
                }
            }
            _ => {}
        }
    }

    event_created_child!(State, wl_data_device::WlDataDevice, [
        wl_data_device::EVT_DATA_OFFER_OPCODE => (wl_data_offer::WlDataOffer, OfferData::default()),
    ]);
}

impl Dispatch<wl_data_offer::WlDataOffer, OfferData> for State {
    fn event(_: &mut State, _: &wl_data_offer::WlDataOffer, e: wl_data_offer::Event, d: &OfferData, _: &Connection, _: &QueueHandle<State>) {
        match e {
            wl_data_offer::Event::Offer { mime_type } => {
                if let Ok(mut m) = d.mimes.lock() {
                    m.push(mime_type);
                }
            }
            wl_data_offer::Event::SourceActions { source_actions: WEnum::Value(a) } => {
                if let Ok(mut bits) = d.source_actions.lock() {
                    *bits = a.bits();
                }
            }
            wl_data_offer::Event::Action { dnd_action: WEnum::Value(a) } => {
                if let Ok(mut bits) = d.action.lock() {
                    *bits = a.bits();
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_data_source::WlDataSource, ()> for State {
    fn event(s: &mut State, src: &wl_data_source::WlDataSource, e: wl_data_source::Event, _: &(), _: &Connection, _: &QueueHandle<State>) {
        match e {
            wl_data_source::Event::Send { mime_type, fd } => {
                let from = match &s.finishing_source {
                    Some((finishing, payload)) if finishing == src => Some(payload),
                    _ if s.dnd_source.as_ref() == Some(src) => s.dnd_payload.as_ref(),
                    _ => s.offer.as_ref(),
                };
                let Some(data) = from.and_then(|o| o.mimes.iter().find(|(m, _)| *m == mime_type).map(|(_, d)| d.clone())) else { return };
                // A separate thread: a reader that stalls mustn't stall the window.
                let _ = std::thread::Builder::new().name("gannet-clip".into()).spawn(move || {
                    let mut f = std::fs::File::from(fd);
                    let _ = std::io::Write::write_all(&mut f, &data);
                });
            }
            // The drag is over for the user, but the source has to live until the receiver has
            // read from it, so clicking right after a drop can't lose the drop.
            wl_data_source::Event::DndDropPerformed => {
                if s.dnd_source.as_ref() == Some(src)
                    && let (Some(source), Some(payload)) = (s.dnd_source.take(), s.dnd_payload.take())
                {
                    if let Some((older, _)) = s.finishing_source.replace((source, payload)) {
                        older.destroy();
                    }
                    s.drop_drag_icon();
                    s.inputs.push_back(Input::OutDragDropped);
                }
            }
            // The other program has what it needs by now. Gannet never deletes anything
            // because of what it reports, and any deleting is the other program's job.
            wl_data_source::Event::DndFinished => {
                if s.finishing_source.as_ref().is_some_and(|(f, _)| f == src) {
                    s.finishing_source = None;
                } else if s.dnd_source.as_ref() == Some(src) {
                    s.dnd_source = None;
                    s.dnd_payload = None;
                    s.drop_drag_icon();
                    s.inputs.push_back(Input::OutDragEnded);
                }
                src.destroy();
            }
            wl_data_source::Event::Cancelled => {
                src.destroy();
                if s.finishing_source.as_ref().is_some_and(|(f, _)| f == src) {
                    s.finishing_source = None;
                } else if s.dnd_source.as_ref() == Some(src) {
                    s.dnd_source = None;
                    s.dnd_payload = None;
                    s.drop_drag_icon();
                    s.inputs.push_back(Input::OutDragEnded);
                } else if s.source.as_ref() == Some(src) {
                    s.source = None;
                    s.offer = None;
                    s.inputs.push_back(Input::ClipboardLost);
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_drop_never_agrees_to_a_move_when_a_copy_is_on_offer() {
        // The drop is finished before the files arrive, so an agreed move could lose them.
        assert_eq!(allowed_actions(true), DndAction::Copy);
        assert!(allowed_actions(false).contains(DndAction::Move), "or a move-only source has nothing to agree on");
    }

    #[test]
    fn a_drag_picture_is_rounded_up_to_a_multiple_of_the_scale() {
        assert_eq!((round_up(121, 2), round_up(120, 2), round_up(121, 3), round_up(7, 1)), (122, 120, 123, 7));
        let icon = DragIcon { w: 3, h: 1, pixels: vec![1, 2, 3] };
        let mut px = vec![0; 8];
        copy_into_larger(&icon, &mut px, 4);
        assert_eq!(px, [1, 2, 3, 1, 1, 1, 1, 1]);
    }
}
