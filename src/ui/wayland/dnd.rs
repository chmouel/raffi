//! Wayland drag-and-drop source support for the file browser.
//!
//! iced's winit backend owns the Wayland connection, so this module attaches
//! to it with [`Backend::from_foreign_display`] (the same trick iced's own
//! clipboard support uses through smithay-clipboard) and runs a worker thread
//! with a private event queue. The worker:
//!
//! - binds its own `wl_seat` / `wl_pointer` so it can observe button-press
//!   serials — every compositor multicasts pointer events to all pointer
//!   resources of the focused client, so this works even though winit owns
//!   the "real" pointer,
//! - waits for the UI to arm a drag with a file path (a press on a file
//!   browser row),
//! - once the pointer moves past a small threshold while the button is still
//!   held, creates a `wl_data_source` offering `text/uri-list` and plain
//!   text, and starts the session with `wl_data_device.start_drag`,
//! - serves the data when the drop target asks for it and reports the
//!   outcome back to the UI through a [`DragEvent`] stream.
//!
//! Dragging out of raffi only works in a native Wayland session; on any other
//! windowing system the worker is never started and pressing rows simply
//! activates them.
//!
//! Safety: the raw `wl_display` / `wl_surface` pointers come from the live
//! iced window. On the normal exit paths (`iced::exit()`, window close) this
//! is verified safe: iced_winit processes `Event::Exit` before the event
//! loop exits, so the program state (and thus the [`DragSource`]) drops
//! while winit still owns the display. On abnormal termination
//! (`Control::Crash` from a renderer/compositor failure, or a main-thread
//! panic) iced_winit drops its `OwnedDisplayHandle` — the `Connection`
//! owning the `wl_display` — before the program state, so [`DragSource::drop`]
//! only waits a bounded time and then detaches the worker; a residual race
//! inherent to iced's teardown remains (root cause upstream in iced_winit).

use std::ffi::c_void;
use std::fs::File;
use std::io::Write;
use std::os::fd::OwnedFd;
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use calloop::channel::{self, Sender};
use calloop::timer::{TimeoutAction, Timer};
use calloop::{EventLoop, LoopHandle, RegistrationToken};
use calloop_wayland_source::WaylandSource;
use iced::futures::channel::mpsc::{UnboundedReceiver, UnboundedSender};
use iced::window::Window;
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::wl_callback::{self, WlCallback};
use wayland_client::protocol::wl_data_device::{self, WlDataDevice};
use wayland_client::protocol::wl_data_device_manager::{DndAction, WlDataDeviceManager};
use wayland_client::protocol::wl_data_offer::{self, WlDataOffer};
use wayland_client::protocol::wl_data_source::{self, WlDataSource};
use wayland_client::protocol::wl_pointer::{self, ButtonState, WlPointer};
use wayland_client::protocol::wl_registry;
use wayland_client::protocol::wl_seat::{self, WlSeat};
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{backend, Connection, Dispatch, Proxy, QueueHandle, WEnum};

/// Mouse button code for the left button (`BTN_LEFT` from linux/input.h).
const BTN_LEFT: u32 = 0x110;

/// Distance in logical surface pixels the pointer must travel while a button
/// is held before a press counts as a drag instead of a click.
const DRAG_THRESHOLD: f64 = 8.0;

/// After `dnd_finished`, a target may still request the data. Give it a short
/// grace period before raffi exits and tears down the connection.
const FINISH_TIMEOUT: Duration = Duration::from_millis(400);

const MIME_URI_LIST: &str = "text/uri-list";
const MIME_TEXT_PLAIN_UTF8: &str = "text/plain;charset=utf-8";
const MIME_UTF8_STRING: &str = "UTF8_STRING";

/// Raw Wayland handles of the main window, carried as `usize` so they can
/// cross threads. Only valid while the iced window lives.
#[derive(Debug, Clone)]
pub(crate) struct RawWaylandHandles {
    pub display: usize,
    pub surface: usize,
}

/// Outcome of a drag-and-drop session, reported to the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DragEvent {
    /// The compositor accepted `start_drag` and grabbed the pointer.
    Started,
    /// The file was dropped on a target that accepted it.
    Dropped,
    /// The drag was aborted: dropped nowhere, rejected, or never started.
    Cancelled,
}

/// Commands sent from the UI thread to the worker.
enum DragCommand {
    /// The user pressed the left button over a file browser row.
    Arm(String),
    /// The pressed row is gone (the listing changed); forget it.
    Disarm,
}

/// How long [`DragSource::drop`] waits for the worker to exit before
/// detaching it.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

/// Handle to the drag worker, owned by the app.
pub(crate) struct DragSource {
    cmd: Option<Sender<DragCommand>>,
    worker: Option<JoinHandle<()>>,
    /// Signals that the worker thread has finished, on every exit path.
    ack_rx: mpsc::Receiver<()>,
}

impl DragSource {
    /// Arm a drag for `path`: the next left-button press-and-move over the
    /// window starts a drag-and-drop session with that file.
    pub(crate) fn arm(&self, path: String) {
        if let Some(cmd) = &self.cmd {
            let _ = cmd.send(DragCommand::Arm(path));
        }
    }

    /// Forget an armed drag.
    pub(crate) fn disarm(&self) {
        if let Some(cmd) = &self.cmd {
            let _ = cmd.send(DragCommand::Disarm);
        }
    }
}

impl Drop for DragSource {
    fn drop(&mut self) {
        // Drop the sender first so the worker's channel reports `Closed`,
        // which makes it exit its loop.
        self.cmd = None;
        // Wait a bounded time for the worker to acknowledge exit: on
        // abnormal termination iced may already have freed the Wayland
        // connection, and an unresponsive worker must not hang teardown.
        // Dropping the JoinHandle without joining detaches the thread.
        match self.ack_rx.recv_timeout(SHUTDOWN_TIMEOUT) {
            Ok(()) => {
                if let Some(worker) = self.worker.take() {
                    let _ = worker.join();
                }
            }
            Err(_) => {
                crate::debug_log!("dnd: worker did not exit within 2s; detaching");
                let _ = self.worker.take();
            }
        }
    }
}

/// Extract the raw Wayland handles from the iced main window, if it runs on
/// Wayland. Runs inside `iced::window::run` on the main thread.
pub(crate) fn extract_wayland_handles(window: &dyn Window) -> Option<RawWaylandHandles> {
    use iced::window::raw_window_handle::{RawDisplayHandle, RawWindowHandle};

    let display = window.display_handle().ok()?.as_raw();
    let surface = window.window_handle().ok()?.as_raw();
    let (RawDisplayHandle::Wayland(display), RawWindowHandle::Wayland(surface)) =
        (display, surface)
    else {
        return None;
    };

    Some(RawWaylandHandles {
        display: display.display.as_ptr() as usize,
        surface: surface.surface.as_ptr() as usize,
    })
}

/// Raw pointers are not `Send`; this wrapper moves them into the worker
/// thread, the only thread that will dereference them (see the module docs
/// for the lifetime argument).
struct SendHandles(*mut c_void, *mut c_void);
unsafe impl Send for SendHandles {}

/// Spawn the drag worker attached to the given window handles. Returns the
/// handle plus the stream of session events to forward into the app.
pub(crate) fn spawn(handles: RawWaylandHandles) -> (DragSource, UnboundedReceiver<DragEvent>) {
    let (event_tx, event_rx) = iced::futures::channel::mpsc::unbounded();
    let (cmd_tx, cmd_rx) = channel::channel::<DragCommand>();
    let (ack_tx, ack_rx) = mpsc::channel::<()>();

    // Raw pointers are not `Send`; the only thread that will dereference
    // them is the worker we are about to spawn (see the module docs for the
    // lifetime argument).
    let worker = std::thread::Builder::new()
        .name("raffi-dnd".to_string())
        .spawn({
            let handles = SendHandles(
                handles.display as *mut c_void,
                handles.surface as *mut c_void,
            );
            move || {
                worker_main(handles, cmd_rx, event_tx);
                // Last action, so the ack fires on every exit path —
                // including worker_main's early returns.
                let _ = ack_tx.send(());
            }
        })
        .ok();

    let source = DragSource {
        cmd: worker.as_ref().map(|_| cmd_tx),
        worker,
        ack_rx,
    };
    (source, event_rx)
}

fn worker_main(
    handles: SendHandles,
    commands: channel::Channel<DragCommand>,
    events: UnboundedSender<DragEvent>,
) {
    let SendHandles(display, surface) = handles;
    // SAFETY: the pointers come from the live iced window; see the module
    // docs for why they outlive this thread.
    let backend = unsafe { backend::Backend::from_foreign_display(display.cast()) };
    let conn = Connection::from_backend(backend);

    let (globals, queue) = match registry_queue_init::<Worker>(&conn) {
        Ok(it) => it,
        Err(error) => {
            crate::debug_log!("dnd: cannot init registry on foreign display: {error}");
            return;
        }
    };
    let qh = queue.handle();

    // A v3 data device manager is needed for drag actions; the seat only
    // needs v1 pointer events (button serials and motion coordinates).
    let Ok(dm) = globals.bind::<WlDataDeviceManager, _, _>(&qh, 1..=3, ()) else {
        crate::debug_log!("dnd: compositor has no wl_data_device_manager");
        return;
    };
    let Ok(seat) = globals.bind::<WlSeat, _, _>(&qh, 1..=9, ()) else {
        crate::debug_log!("dnd: compositor has no wl_seat");
        return;
    };
    let data_device = dm.get_data_device(&seat, &qh, ());
    // Bind our own pointer: compositors deliver a copy of every pointer event
    // to all pointer resources of the focused client, which is how we learn
    // the serial of the button press that must authorise `start_drag`.
    let _pointer = seat.get_pointer(&qh, ());

    let surface_id =
        match unsafe { backend::ObjectId::from_ptr(WlSurface::interface(), surface.cast()) } {
            Ok(id) => id,
            Err(_) => {
                crate::debug_log!("dnd: main window surface is not a wl_surface");
                return;
            }
        };

    let Ok(mut event_loop) = EventLoop::<Worker>::try_new() else {
        return;
    };
    let handle = event_loop.handle();

    let mut worker = Worker {
        conn: conn.clone(),
        dm,
        data_device,
        surface_id,
        handle: handle.clone(),
        pointer_pos: (0.0, 0.0),
        press: None,
        armed: None,
        active: None,
        finishing: None,
        sync: None,
        finish_timer: None,
        offer: None,
        events,
        done: false,
    };

    if handle
        .insert_source(commands, |event, _, worker| match event {
            channel::Event::Msg(command) => match command {
                DragCommand::Arm(path) => worker.armed = Some(path),
                DragCommand::Disarm => worker.armed = None,
            },
            channel::Event::Closed => worker.done = true,
        })
        .is_err()
    {
        return;
    }

    if WaylandSource::new(conn, queue).insert(handle).is_err() {
        return;
    }

    loop {
        if event_loop.dispatch(None, &mut worker).is_err() || worker.done {
            break;
        }
    }

    if let Some(drag) = worker.active.take() {
        drag.source.destroy();
    }
    if let Some(drag) = worker.finishing.take() {
        drag.source.destroy();
    }
    let _ = worker.events.unbounded_send(DragEvent::Cancelled);
}

struct Worker {
    conn: Connection,
    dm: WlDataDeviceManager,
    data_device: WlDataDevice,
    surface_id: backend::ObjectId,
    handle: LoopHandle<'static, Worker>,
    pointer_pos: (f64, f64),
    press: Option<Press>,
    armed: Option<String>,
    active: Option<ActiveDrag>,
    /// A finished drag whose target has not requested the data yet.
    finishing: Option<ActiveDrag>,
    /// `wl_display.sync` callback proving whether the compositor accepted
    /// the last `start_drag` request.
    sync: Option<WlCallback>,
    finish_timer: Option<RegistrationToken>,
    /// Data offer currently hovering our own window, if any.
    offer: Option<WlDataOffer>,
    events: UnboundedSender<DragEvent>,
    done: bool,
}

#[derive(Debug, Clone, Copy)]
struct Press {
    serial: u32,
    x: f64,
    y: f64,
}

struct ActiveDrag {
    source: WlDataSource,
    /// The compositor has proven the session is live.
    confirmed: bool,
    /// The target has read the data.
    sent: bool,
    path: String,
}

impl Worker {
    fn emit(&self, event: DragEvent) {
        let _ = self.events.unbounded_send(event);
    }

    /// The compositor produced an event that can only occur while one of our
    /// drag sessions is live.
    fn confirm_drag(&mut self) {
        if let Some(drag) = self.active.as_mut() {
            drag.confirmed = true;
        }
    }

    fn remove_timer(&mut self) {
        if let Some(token) = self.finish_timer.take() {
            self.handle.remove(token);
        }
    }

    /// Start the drag session authorised by the press `serial`.
    fn start_drag(&mut self, path: String, serial: u32, qh: &QueueHandle<Self>) {
        // A session that never completed would otherwise linger forever.
        if let Some(stale) = self.active.take() {
            crate::debug_log!("dnd: replacing unconfirmed drag session");
            stale.source.destroy();
            self.emit(DragEvent::Cancelled);
        }

        let source = self.dm.create_data_source(qh, ());
        for mime in [MIME_URI_LIST, MIME_TEXT_PLAIN_UTF8, MIME_UTF8_STRING] {
            source.offer(mime.to_string());
        }
        if self.dm.version() >= 3 {
            source.set_actions(DndAction::Copy);
        }

        let surface = match WlSurface::from_id(&self.conn, self.surface_id.clone()) {
            Ok(surface) => surface,
            Err(_) => return,
        };

        self.data_device
            .start_drag(Some(&source), &surface, None, serial);

        // Compositors silently ignore `start_drag` when the serial does not
        // match a live implicit grab (a fast click can already have released
        // the button compositor-side). A `wl_display.sync` issued right after
        // the request detects this deterministically: an accepted drag
        // synchronously produces events on our queue (the DnD grab sending
        // `wl_pointer.leave` to the origin surface, or `wl_data_source`
        // events) that are ordered before the sync callback. If the callback
        // arrives without any of them, the request was dropped.
        self.sync = Some(self.conn.display().sync(qh, ()));

        let _ = self.conn.flush();

        self.active = Some(ActiveDrag {
            source,
            confirmed: false,
            sent: false,
            path,
        });
        self.press = None;
        self.armed = None;
        self.emit(DragEvent::Started);
    }

    fn on_sync_done(&mut self) {
        self.sync = None;
        let Some(drag) = self.active.as_ref() else {
            return;
        };
        if drag.confirmed {
            return;
        }
        crate::debug_log!("dnd: compositor ignored start_drag, cancelling session");
        let Some(drag) = self.active.take() else {
            return;
        };
        drag.source.destroy();
        self.emit(DragEvent::Cancelled);
    }

    /// End a drag session. `dropped` is true when the target accepted it.
    fn finish_drag(&mut self, dropped: bool) {
        self.sync = None;
        if !dropped {
            if let Some(drag) = self.active.take() {
                drag.source.destroy();
            }
            if let Some(drag) = self.finishing.take() {
                drag.source.destroy();
                self.remove_timer();
            }
            self.emit(DragEvent::Cancelled);
            return;
        }

        if let Some(drag) = self.active.take() {
            if drag.sent {
                drag.source.destroy();
                self.emit(DragEvent::Dropped);
            } else {
                // The target may still request the data after `dnd_finished`.
                self.finishing = Some(drag);
                match self.handle.insert_source(
                    Timer::from_duration(FINISH_TIMEOUT),
                    |_, (), worker| {
                        worker.on_finish_timeout();
                        TimeoutAction::Drop
                    },
                ) {
                    Ok(token) => self.finish_timer = Some(token),
                    Err(_) => self.on_finish_timeout(),
                }
            }
        }
    }

    fn on_finish_timeout(&mut self) {
        self.finish_timer = None;
        if let Some(drag) = self.finishing.take() {
            crate::debug_log!("dnd: target never requested the data after finishing");
            drag.source.destroy();
        }
        self.emit(DragEvent::Dropped);
    }

    /// Serve a `wl_data_source.send` request from the drop target.
    fn send_data(&mut self, fd: OwnedFd, mime_type: &str) {
        let path = self
            .active
            .as_ref()
            .map(|drag| drag.path.as_str())
            .or(self.finishing.as_ref().map(|drag| drag.path.as_str()));
        let Some(path) = path else {
            return;
        };
        let contents = content_for_mime(mime_type, path);
        if let Some(drag) = self.active.as_mut() {
            drag.sent = true;
        }
        if self.finishing.is_some() {
            // The data was served; no need to keep the source alive for the
            // rest of the grace period.
            let drag = self.finishing.take().expect("checked above");
            drag.source.destroy();
            self.remove_timer();
            self.emit(DragEvent::Dropped);
        }

        // The fd is ours to serve and close, even on write errors.
        let mut file = File::from(fd);
        let _ = file.write_all(&contents);
    }

    fn maybe_start_drag(&mut self, qh: &QueueHandle<Self>) {
        if self.active.is_some() || self.finishing.is_some() {
            return;
        }
        let Some(path) = self.armed.clone() else {
            return;
        };
        let Some(press) = self.press else {
            return;
        };
        let (dx, dy) = (self.pointer_pos.0 - press.x, self.pointer_pos.1 - press.y);
        if dx * dx + dy * dy < DRAG_THRESHOLD * DRAG_THRESHOLD {
            return;
        }
        crate::debug_log!("dnd: starting drag for {path:?} (serial {})", press.serial);
        self.start_drag(path, press.serial, qh);
    }
}

impl Dispatch<WlCallback, ()> for Worker {
    fn event(
        state: &mut Self,
        _callback: &WlCallback,
        event: wl_callback::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        if matches!(event, wl_callback::Event::Done { .. }) {
            state.on_sync_done();
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Worker {
    fn event(
        _state: &mut Self,
        _registry: &wl_registry::WlRegistry,
        _event: wl_registry::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlSeat, ()> for Worker {
    fn event(
        _state: &mut Self,
        _seat: &WlSeat,
        _event: wl_seat::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlPointer, ()> for Worker {
    fn event(
        state: &mut Self,
        _pointer: &WlPointer,
        event: wl_pointer::Event,
        _data: &(),
        _conn: &Connection,
        qhandle: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter {
                surface_x,
                surface_y,
                ..
            } => {
                state.pointer_pos = (surface_x, surface_y);
            }
            wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => {
                state.pointer_pos = (surface_x, surface_y);
                state.maybe_start_drag(qhandle);
            }
            wl_pointer::Event::Button {
                serial,
                button,
                state: button_state,
                ..
            } => {
                if button != BTN_LEFT {
                    return;
                }
                match button_state {
                    WEnum::Value(ButtonState::Pressed) => {
                        state.press = Some(Press {
                            serial,
                            x: state.pointer_pos.0,
                            y: state.pointer_pos.1,
                        });
                    }
                    WEnum::Value(ButtonState::Released) => {
                        // The press is over; a click that never became a drag.
                        state.press = None;
                        state.armed = None;
                        // A live DnD grab sends `wl_pointer.leave` to the
                        // origin pointer at grab start and consumes all
                        // further button events, so a Release reaching our
                        // snooping pointer proves the compositor never took
                        // the grab (e.g. it silently ignored a stale-serial
                        // `start_drag`). Cancel the unconfirmed session now
                        // instead of waiting for the sync roundtrip, which
                        // an unrelated pointer Leave could otherwise
                        // pre-empt into a false confirmation.
                        if state.active.as_ref().is_some_and(|drag| !drag.confirmed) {
                            let drag = state.active.take().expect("checked above");
                            state.sync = None;
                            drag.source.destroy();
                            state.emit(DragEvent::Cancelled);
                        }
                    }
                    _ => {}
                }
            }
            wl_pointer::Event::Leave { .. } => {
                if state.active.is_some() {
                    // The DnD grab takes the pointer away from the origin
                    // surface, which proves the compositor accepted the drag.
                    state.confirm_drag();
                } else {
                    state.press = None;
                    state.armed = None;
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<WlDataDevice, ()> for Worker {
    wayland_client::event_created_child!(Worker, WlDataDevice, [
        wl_data_device::EVT_DATA_OFFER_OPCODE => (WlDataOffer, ())
    ]);

    fn event(
        state: &mut Self,
        _device: &WlDataDevice,
        event: wl_data_device::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        match event {
            wl_data_device::Event::DataOffer { id } => {
                if let Some(old) = state.offer.replace(id) {
                    old.destroy();
                }
            }
            // A drag is hovering our own window. If it is ours it proves the
            // session is live; either way we leave the offer unanswered so
            // raffi never accepts drops onto itself.
            wl_data_device::Event::Enter { .. } => {
                state.confirm_drag();
            }
            wl_data_device::Event::Leave => {
                if let Some(offer) = state.offer.take() {
                    offer.destroy();
                }
            }
            wl_data_device::Event::Drop => {
                if let Some(offer) = state.offer.take() {
                    offer.destroy();
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<WlDataOffer, ()> for Worker {
    fn event(
        _state: &mut Self,
        _offer: &WlDataOffer,
        _event: wl_data_offer::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlDataSource, ()> for Worker {
    fn event(
        state: &mut Self,
        source: &WlDataSource,
        event: wl_data_source::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        let is_ours = state
            .active
            .as_ref()
            .is_some_and(|drag| drag.source == *source)
            || state
                .finishing
                .as_ref()
                .is_some_and(|drag| drag.source == *source);
        if !is_ours {
            return;
        }

        match event {
            wl_data_source::Event::Target { .. } => state.confirm_drag(),
            wl_data_source::Event::Send { fd, mime_type } => {
                state.confirm_drag();
                state.send_data(fd, &mime_type);
            }
            wl_data_source::Event::DndDropPerformed => state.confirm_drag(),
            wl_data_source::Event::DndFinished => state.finish_drag(true),
            wl_data_source::Event::Cancelled => state.finish_drag(false),
            wl_data_source::Event::Action { .. } => state.confirm_drag(),
            _ => {}
        }
    }
}

impl Dispatch<WlDataDeviceManager, ()> for Worker {
    fn event(
        _state: &mut Self,
        _manager: &WlDataDeviceManager,
        _event: <WlDataDeviceManager as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
    }
}

/// Percent-encode a filesystem path for use inside a `file://` URI.
///
/// Everything outside the URI unreserved set (plus `/`, the path separator)
/// is encoded, which also neutralises `#`, `?` and `%` in file names.
fn encode_uri_path(path: &str) -> String {
    let mut encoded = String::with_capacity(path.len());
    for byte in path.bytes() {
        let character = byte as char;
        if character.is_ascii_alphanumeric() || matches!(character, '-' | '.' | '_' | '~' | '/') {
            encoded.push(character);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

/// Build the `file://` URI for a local path.
fn file_uri(path: &str) -> String {
    format!("file://{}", encode_uri_path(path))
}

/// Bytes served to the drop target for `mime_type`.
fn content_for_mime(mime_type: &str, path: &str) -> Vec<u8> {
    if mime_type == MIME_URI_LIST {
        // The list is CRLF terminated per RFC 2483.
        format!("{}\r\n", file_uri(path)).into_bytes()
    } else {
        // Plain text targets (terminals, editors) receive the bare path.
        path.as_bytes().to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::{content_for_mime, encode_uri_path, file_uri, MIME_URI_LIST};

    #[test]
    fn test_encode_uri_path_keeps_safe_characters() {
        assert_eq!(
            encode_uri_path("/home/user/file.txt"),
            "/home/user/file.txt"
        );
        assert_eq!(encode_uri_path("/a-b_c.d~e"), "/a-b_c.d~e");
    }

    #[test]
    fn test_encode_uri_path_encodes_spaces_and_specials() {
        assert_eq!(
            encode_uri_path("/home/user/My File.txt"),
            "/home/user/My%20File.txt"
        );
        assert_eq!(encode_uri_path("/tmp/a#b?c"), "/tmp/a%23b%3Fc");
        assert_eq!(encode_uri_path("/tmp/50%off"), "/tmp/50%25off");
    }

    #[test]
    fn test_encode_uri_path_encodes_non_ascii_bytes() {
        assert_eq!(encode_uri_path("/tmp/é"), "/tmp/%C3%A9");
    }

    #[test]
    fn test_file_uri() {
        assert_eq!(file_uri("/tmp/x"), "file:///tmp/x");
        assert_eq!(file_uri("/tmp/a b"), "file:///tmp/a%20b");
    }

    #[test]
    fn test_content_for_mime() {
        assert_eq!(
            content_for_mime(MIME_URI_LIST, "/tmp/a b"),
            b"file:///tmp/a%20b\r\n".to_vec()
        );
        assert_eq!(
            content_for_mime("text/plain;charset=utf-8", "/tmp/a b"),
            b"/tmp/a b".to_vec()
        );
        assert_eq!(
            content_for_mime("UTF8_STRING", "/tmp/a b"),
            b"/tmp/a b".to_vec()
        );
    }
}
