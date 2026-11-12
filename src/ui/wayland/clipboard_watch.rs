//! Clipboard recorder for the `raffi` clipboard backend, run inside
//! `raffi --cache-daemon`.
//!
//! The daemon binds the compositor's data-control protocol
//! (`ext-data-control-v1`, else `wlr-data-control-unstable-v1`), which lets a
//! clipboard manager see every new selection without having focus. For each
//! one it reads the formats worth keeping and hands them to a recorder thread
//! that stores them in the history database (see `clipboard_store`), with
//! the copy time and the app that had focus.
//!
//! The daemon also owns the selection when the UI puts a history entry back
//! on the clipboard, so the entry stays pasteable after Raffi closes. Its
//! own selections carry `OWN_MIME`, which keeps them out of the history.
//! When the app that owned the clipboard exits, the daemon puts the entry
//! recorded from it back, so the copy survives the app.

use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::process::Command as Process;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use calloop::channel;
use wayland_client::Connection;

use super::clipboard_store::{Capture, Limits, Skipped, Store, MAX_TEXT_BYTES};
use crate::compositor::Compositor;

/// Marks selections owned by Raffi so they are not recorded again.
const OWN_MIME: &str = "application/x-raffi-clipboard";
/// Set by password managers (KeePassXC, among others) on secrets.
const PASSWORD_HINT: &str = "x-kde-passwordManagerHint";
/// Text formats in order of preference; the first offered one is stored as
/// `text/plain`.
const TEXT_MIMES: [&str; 6] = [
    "text/plain;charset=utf-8",
    "text/plain;charset=UTF-8",
    "text/plain",
    "UTF8_STRING",
    "STRING",
    "TEXT",
];
/// Names `text/plain` is also offered under when an entry is put back.
const TEXT_ALIASES: [&str; 4] = ["text/plain;charset=utf-8", "UTF8_STRING", "STRING", "TEXT"];
const KEPT_MIMES: [&str; 3] = ["text/html", "text/uri-list", "x-special/gnome-copied-files"];
/// Raster formats in order of preference. Sources such as browsers convert
/// one image to every format they offer, so only the best one is kept.
const IMAGE_MIMES: [&str; 5] = [
    "image/png",
    "image/jpeg",
    "image/webp",
    "image/gif",
    "image/bmp",
];
const SVG_MIME: &str = "image/svg+xml";
const READ_TIMEOUT: Duration = Duration::from_secs(10);
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);
const SELECT_TIMEOUT: Duration = Duration::from_secs(3);
const RECONNECT_DELAY: Duration = Duration::from_secs(10);
const APP_TIMEOUT: Duration = Duration::from_secs(1);
/// Selections waiting to be read. Further copies are skipped while the
/// recorder is stuck on a slow source, so their pipes do not pile up.
const MAX_PENDING_JOBS: usize = 8;

/// Requests to the clipboard thread.
enum Command {
    /// From the socket server: put an entry on the clipboard.
    Select {
        hash: String,
        reply: mpsc::Sender<Result<(), String>>,
    },
    /// From the recorder: the selection `generation` was stored as `hash`.
    Recorded { generation: u64, hash: String },
}

static COMMANDS: Mutex<Option<channel::Sender<Command>>> = Mutex::new(None);

/// Formats to read from an offer, as (requested MIME type, stored MIME
/// type), or `None` when the selection must not be recorded.
fn plan_reads(offered: &[String]) -> Option<Vec<(String, String)>> {
    let has = |mime: &str| offered.iter().any(|offer| offer == mime);
    if has(OWN_MIME) || has(PASSWORD_HINT) {
        return None;
    }
    let mut reads = Vec::new();
    if let Some(text) = TEXT_MIMES.into_iter().find(|mime| has(mime)) {
        reads.push((text.to_string(), "text/plain".to_string()));
    }
    for mime in KEPT_MIMES.into_iter().filter(|mime| has(mime)) {
        reads.push((mime.to_string(), mime.to_string()));
    }
    let raster = IMAGE_MIMES
        .into_iter()
        .find(|mime| has(mime))
        .map(str::to_string)
        .or_else(|| {
            offered
                .iter()
                .find(|mime| mime.starts_with("image/") && *mime != SVG_MIME)
                .cloned()
        });
    for image in raster
        .into_iter()
        .chain(has(SVG_MIME).then(|| SVG_MIME.into()))
    {
        reads.push((image.clone(), image));
    }
    (!reads.is_empty()).then_some(reads)
}

/// MIME types to offer for an entry with the stored `formats`.
fn offered_mimes(formats: &[(String, Vec<u8>)]) -> Vec<String> {
    let mut mimes = Vec::new();
    for (mime, _) in formats {
        mimes.push(mime.clone());
        if mime == "text/plain" {
            mimes.extend(TEXT_ALIASES.iter().map(|alias| alias.to_string()));
        }
    }
    mimes.push(OWN_MIME.to_string());
    mimes
}

/// The stored bytes a paste target asking for `mime` gets.
fn data_for<'a>(formats: &'a [(String, Vec<u8>)], mime: &str) -> Option<&'a [u8]> {
    let wanted = if TEXT_ALIASES.contains(&mime) {
        "text/plain"
    } else {
        mime
    };
    formats
        .iter()
        .find(|(stored, _)| stored == wanted)
        .map(|(_, data)| data.as_slice())
}

fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: pipe2 just returned these two descriptors and nothing else owns them.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

fn set_nonblocking(fd: BorrowedFd<'_>) -> io::Result<()> {
    let raw = fd.as_raw_fd();
    let flags = unsafe { libc::fcntl(raw, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(raw, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Wait until `fd` is ready for `events`, or fail at `deadline`.
fn wait_ready(fd: BorrowedFd<'_>, events: libc::c_short, deadline: Instant) -> io::Result<()> {
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "timed out"));
        }
        let mut poll = libc::pollfd {
            fd: fd.as_raw_fd(),
            events,
            revents: 0,
        };
        let millis = left.as_millis().clamp(1, i32::MAX as u128) as i32;
        match unsafe { libc::poll(&mut poll, 1, millis) } {
            0 => continue,
            n if n > 0 => return Ok(()),
            _ => {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
        }
    }
}

/// Read a source's data from the pipe until it closes, refusing more than
/// `cap` bytes.
fn read_pipe(fd: OwnedFd, cap: u64, deadline: Instant) -> Result<Vec<u8>, String> {
    set_nonblocking(fd.as_fd()).map_err(|error| error.to_string())?;
    let mut data = Vec::new();
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        wait_ready(fd.as_fd(), libc::POLLIN, deadline).map_err(|error| error.to_string())?;
        let read = unsafe { libc::read(fd.as_raw_fd(), chunk.as_mut_ptr().cast(), chunk.len()) };
        match read {
            0 => return Ok(data),
            n if n > 0 => {
                data.extend_from_slice(&chunk[..n as usize]);
                if data.len() as u64 > cap {
                    return Err(format!("more than {cap} bytes"));
                }
            }
            _ => {
                let error = io::Error::last_os_error();
                if !matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) {
                    return Err(error.to_string());
                }
            }
        }
    }
}

/// Serve `data` to a paste target, giving up if it stops reading.
fn write_pipe(fd: OwnedFd, data: &[u8], deadline: Instant) -> io::Result<()> {
    set_nonblocking(fd.as_fd())?;
    let mut rest = data;
    while !rest.is_empty() {
        wait_ready(fd.as_fd(), libc::POLLOUT, deadline)?;
        let written = unsafe { libc::write(fd.as_raw_fd(), rest.as_ptr().cast(), rest.len()) };
        if written < 0 {
            let error = io::Error::last_os_error();
            if matches!(
                error.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
            ) {
                continue;
            }
            return Err(error);
        }
        rest = &rest[written as usize..];
    }
    Ok(())
}

/// App ID or window class of the focused window, from the compositor's
/// JSON reply to `focused_app_command`.
fn focused_app_from(compositor: Compositor, output: &[u8]) -> Option<String> {
    fn sway_focused(node: &serde_json::Value) -> Option<String> {
        if node.get("focused").and_then(serde_json::Value::as_bool) == Some(true) {
            return node
                .get("app_id")
                .and_then(serde_json::Value::as_str)
                .or_else(|| node.pointer("/window_properties/class")?.as_str())
                .map(str::to_string);
        }
        ["nodes", "floating_nodes"]
            .iter()
            .filter_map(|key| node.get(key)?.as_array())
            .flatten()
            .find_map(sway_focused)
    }

    let value: serde_json::Value = serde_json::from_slice(output).ok()?;
    let app = match compositor {
        Compositor::Hyprland => value.get("class")?.as_str().map(str::to_string),
        Compositor::Niri => value.get("app_id")?.as_str().map(str::to_string),
        Compositor::Sway => sway_focused(&value),
    }?;
    let app = app.trim();
    (!app.is_empty()).then(|| app.to_string())
}

fn focused_app_command(compositor: Compositor) -> &'static [&'static str] {
    match compositor {
        Compositor::Hyprland => &["activewindow", "-j"],
        Compositor::Niri => &["msg", "--json", "focused-window"],
        Compositor::Sway => &["-t", "get_tree", "-r"],
    }
}

fn focused_app() -> Option<String> {
    let compositor = Compositor::current()?;
    let mut command = Process::new(compositor.program());
    command.args(focused_app_command(compositor));
    let output = super::actions::run_bounded(command, APP_TIMEOUT, 16 * 1024 * 1024).ok()?;
    focused_app_from(compositor, &output)
}

/// Formats being read from one selection.
struct Job {
    generation: u64,
    reads: Vec<(String, OwnedFd)>,
    copied_at: i64,
    /// The app focused when the copy happened, looked up in the background.
    source: mpsc::Receiver<Option<String>>,
}

/// Tracks which history entry holds the current selection, so it can be put
/// back when its source app exits and the selection becomes empty.
#[derive(Default)]
struct Persist {
    /// Numbers selections, so late recordings of older ones are ignored.
    generation: u64,
    recorded: Option<(u64, String)>,
    /// The current selection was cleared before it was recorded.
    cleared: bool,
}

impl Persist {
    /// A new selection arrived; `own` is the entry Raffi itself offers.
    fn selection(&mut self, own: Option<String>) -> u64 {
        // Shared across reconnections, so recordings queued before one never
        // match a selection after it.
        static GENERATION: AtomicU64 = AtomicU64::new(0);
        self.generation = GENERATION.fetch_add(1, Ordering::Relaxed) + 1;
        self.cleared = false;
        self.recorded = own.map(|hash| (self.generation, hash));
        self.generation
    }

    /// The selection became empty: the entry to restore, if recorded yet.
    fn cleared(&mut self) -> Option<String> {
        self.cleared = true;
        self.current()
    }

    /// Selection `generation` was stored: the entry to restore when the
    /// selection was cleared in the meantime.
    fn recorded(&mut self, generation: u64, hash: String) -> Option<String> {
        if generation != self.generation {
            return None;
        }
        self.recorded = Some((generation, hash));
        if self.cleared {
            self.current()
        } else {
            None
        }
    }

    fn current(&self) -> Option<String> {
        self.recorded
            .as_ref()
            .filter(|(generation, _)| *generation == self.generation)
            .map(|(_, hash)| hash.clone())
    }
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs() as i64)
}

/// A text format among the `failed` reads, when exclusions are active. Its
/// text might match them, and the formats that were read might not show it,
/// so the capture must not be recorded.
fn unchecked_text_format<'a>(failed: &'a [String], exclude: &[regex::Regex]) -> Option<&'a str> {
    if exclude.is_empty() {
        return None;
    }
    failed
        .iter()
        .find(|mime| !mime.starts_with("image/"))
        .map(String::as_str)
}

/// Read and store each job in turn.
fn record(jobs: mpsc::Receiver<Job>, limits: Limits, exclude: Vec<regex::Regex>) {
    let mut store = match Store::open_default() {
        Ok(store) => store,
        Err(error) => {
            eprintln!("raffi: clipboard history unavailable: {error}");
            return;
        }
    };
    for job in jobs {
        let deadline = Instant::now() + READ_TIMEOUT;
        let mut formats = Vec::new();
        let mut failed = Vec::new();
        // Sources usually answer requests in order, so read them in order.
        // A format that fails is left out; the others are still kept.
        for (mime, fd) in job.reads {
            let cap = if mime.starts_with("image/") {
                limits.max_image_size
            } else {
                MAX_TEXT_BYTES as u64
            };
            match read_pipe(fd, cap, deadline) {
                Ok(data) => formats.push((mime, data)),
                Err(error) => {
                    eprintln!("raffi: clipboard {mime} not recorded: {error}");
                    failed.push(mime);
                }
            }
        }
        if let Some(mime) = unchecked_text_format(&failed, &exclude) {
            eprintln!(
                "raffi: clipboard entry not recorded: its {mime} could not be read to check exclude_patterns"
            );
            continue;
        }
        let capture = Capture {
            formats,
            copied_at: job.copied_at,
            source: job.source.recv_timeout(APP_TIMEOUT * 2).ok().flatten(),
        };
        match store.insert_filtered(capture, &limits, &exclude) {
            Ok(hash) => {
                let commands = COMMANDS.lock().unwrap_or_else(|error| error.into_inner());
                if let Some(commands) = commands.as_ref() {
                    let _ = commands.send(Command::Recorded {
                        generation: job.generation,
                        hash,
                    });
                }
            }
            Err(Skipped::Empty | Skipped::Excluded) => {}
            Err(error) => eprintln!("raffi: clipboard entry not recorded: {error}"),
        }
    }
}

/// Stored MIME types with their data.
type Formats = Vec<(String, Vec<u8>)>;

/// State shared by both protocol implementations.
struct Core {
    store: Store,
    jobs: mpsc::SyncSender<Job>,
    /// The entry Raffi currently owns the selection with, and its formats.
    served: Option<(String, Arc<Formats>)>,
    persist: Persist,
}

impl Core {
    /// Request the formats worth keeping from a new selection. `receive`
    /// asks the source to write one format to a pipe. Returns the write ends
    /// to close once the requests are flushed.
    fn capture(
        &mut self,
        offered: &[String],
        mut receive: impl FnMut(String, BorrowedFd<'_>),
    ) -> Vec<OwnedFd> {
        let own = offered
            .iter()
            .any(|mime| mime == OWN_MIME)
            .then(|| self.served.as_ref().map(|(hash, _)| hash.clone()))
            .flatten();
        let generation = self.persist.selection(own);
        let Some(plan) = plan_reads(offered) else {
            return Vec::new();
        };
        let mut reads = Vec::new();
        let mut writers = Vec::new();
        for (request, stored) in plan {
            let Ok((read, write)) = pipe() else {
                return Vec::new();
            };
            receive(request, write.as_fd());
            reads.push((stored, read));
            writers.push(write);
        }
        let (found, source) = mpsc::channel();
        let job = Job {
            generation,
            reads,
            copied_at: now_secs(),
            source,
        };
        if self.jobs.try_send(job).is_err() {
            eprintln!("raffi: clipboard recorder is busy, a copy was not recorded");
            return writers;
        }
        // Look up the app now, while it still has focus.
        let _ = std::thread::Builder::new()
            .name("raffi-clipboard-app".into())
            .spawn(move || {
                let _ = found.send(focused_app());
            });
        writers
    }

    /// Load the entry `hash` to serve it, returning the MIME types to offer.
    fn load(&mut self, hash: &str) -> Result<Vec<String>, String> {
        let formats = self.store.formats(hash)?;
        let mimes = offered_mimes(&formats);
        self.served = Some((hash.to_string(), Arc::new(formats)));
        Ok(mimes)
    }

    fn send(&self, mime: &str, fd: OwnedFd) {
        let Some(formats) = self.served.as_ref().map(|(_, formats)| formats.clone()) else {
            return;
        };
        let mime = mime.to_string();
        std::thread::spawn(move || {
            let data = data_for(&formats, &mime).unwrap_or_default();
            if let Err(error) = write_pipe(fd, data, Instant::now() + WRITE_TIMEOUT) {
                crate::debug_log!("clipboard: serving {mime} failed: {error}");
            }
        });
    }
}

/// How a protocol run ended.
enum RunError {
    /// The compositor lacks this protocol.
    Unsupported,
    Failed(String),
}

macro_rules! data_control {
    (
        $name:ident,
        [$($proto:ident)::+],
        $manager:ident :: $Manager:ident,
        $device:ident :: $Device:ident,
        $offer:ident :: $Offer:ident,
        $source:ident :: $Source:ident,
        $versions:expr
    ) => {
        mod $name {
            use std::sync::Mutex;

            use calloop::channel;
            use calloop::EventLoop;
            use calloop_wayland_source::WaylandSource;
            use wayland_client::globals::{registry_queue_init, GlobalListContents};
            use wayland_client::protocol::{wl_registry, wl_seat::{self, WlSeat}};
            use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
            use $($proto)::+::{
                $device::{self, $Device},
                $manager::{self, $Manager},
                $offer::{self, $Offer},
                $source::{self, $Source},
            };

            use super::{Command, Core, RunError, COMMANDS};

            type Mimes = Mutex<Vec<String>>;

            struct State {
                core: Core,
                conn: Connection,
                qh: QueueHandle<State>,
                manager: $Manager,
                device: $Device,
                source: Option<$Source>,
                done: bool,
            }

            pub(super) fn run(conn: &Connection, core: impl FnOnce() -> Result<Core, String>) -> Result<(), RunError> {
                let (globals, queue) = registry_queue_init::<State>(conn)
                    .map_err(|error| RunError::Failed(error.to_string()))?;
                let qh = queue.handle();
                let manager = globals
                    .bind::<$Manager, _, _>(&qh, $versions, ())
                    .map_err(|_| RunError::Unsupported)?;
                let seat = globals
                    .bind::<WlSeat, _, _>(&qh, 1..=1, ())
                    .map_err(|_| RunError::Failed("the compositor has no seat".into()))?;
                let core = core().map_err(RunError::Failed)?;
                let device = manager.get_data_device(&seat, &qh, ());
                let mut event_loop = EventLoop::<State>::try_new()
                    .map_err(|error| RunError::Failed(error.to_string()))?;
                let handle = event_loop.handle();
                let (sender, commands) = channel::channel::<Command>();
                handle
                    .insert_source(commands, |event, _, state| {
                        if let channel::Event::Msg(command) = event {
                            state.handle(command);
                        }
                    })
                    .map_err(|error| RunError::Failed(error.to_string()))?;
                WaylandSource::new(conn.clone(), queue)
                    .insert(handle)
                    .map_err(|error| RunError::Failed(error.to_string()))?;
                *COMMANDS.lock().unwrap_or_else(|error| error.into_inner()) = Some(sender);

                let mut state = State {
                    core,
                    conn: conn.clone(),
                    qh,
                    manager,
                    device,
                    source: None,
                    done: false,
                };
                let result = loop {
                    if let Err(error) = event_loop.dispatch(None, &mut state) {
                        break Err(RunError::Failed(error.to_string()));
                    }
                    if state.done {
                        break Err(RunError::Failed("the compositor stopped the clipboard device".into()));
                    }
                };
                *COMMANDS.lock().unwrap_or_else(|error| error.into_inner()) = None;
                result
            }

            impl State {
                fn handle(&mut self, command: Command) {
                    match command {
                        Command::Select { hash, reply } => {
                            let result = self.select(&hash);
                            let _ = reply.send(result);
                        }
                        Command::Recorded { generation, hash } => {
                            if let Some(hash) = self.core.persist.recorded(generation, hash) {
                                self.restore(&hash);
                            }
                        }
                    }
                }

                /// Put back the entry whose source app left the clipboard empty.
                fn restore(&mut self, hash: &str) {
                    if let Err(error) = self.select(hash) {
                        eprintln!("raffi: cannot keep the last copy on the clipboard: {error}");
                    }
                }

                fn select(&mut self, hash: &str) -> Result<(), String> {
                    let mimes = self.core.load(hash)?;
                    let source = self.manager.create_data_source(&self.qh, ());
                    for mime in mimes {
                        source.offer(mime);
                    }
                    self.device.set_selection(Some(&source));
                    if let Some(old) = self.source.replace(source) {
                        old.destroy();
                    }
                    self.conn.flush().map_err(|error| error.to_string())?;
                    self.core.store.bump(hash)
                }
            }

            impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
                fn event(_: &mut Self, _: &wl_registry::WlRegistry, _: wl_registry::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
            }

            impl Dispatch<WlSeat, ()> for State {
                fn event(_: &mut Self, _: &WlSeat, _: wl_seat::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
            }

            impl Dispatch<$Manager, ()> for State {
                fn event(_: &mut Self, _: &$Manager, _: $manager::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
            }

            impl Dispatch<$Offer, Mimes> for State {
                fn event(_: &mut Self, _: &$Offer, event: $offer::Event, mimes: &Mimes, _: &Connection, _: &QueueHandle<Self>) {
                    if let $offer::Event::Offer { mime_type } = event {
                        mimes.lock().unwrap_or_else(|error| error.into_inner()).push(mime_type);
                    }
                }
            }

            impl Dispatch<$Device, ()> for State {
                wayland_client::event_created_child!(State, $Device, [
                    $device::EVT_DATA_OFFER_OPCODE => ($Offer, Mimes::default())
                ]);

                fn event(state: &mut Self, _: &$Device, event: $device::Event, _: &(), conn: &Connection, _: &QueueHandle<Self>) {
                    match event {
                        $device::Event::Selection { id: Some(offer) } => {
                            let offered = offer
                                .data::<Mimes>()
                                .map(|mimes| mimes.lock().unwrap_or_else(|error| error.into_inner()).clone())
                                .unwrap_or_default();
                            let writers = state.core.capture(&offered, |mime, fd| offer.receive(mime, fd));
                            let _ = conn.flush();
                            drop(writers);
                            offer.destroy();
                        }
                        $device::Event::Selection { id: None } => {
                            if let Some(hash) = state.core.persist.cleared() {
                                state.restore(&hash);
                            }
                        }
                        $device::Event::PrimarySelection { id: Some(offer) } => offer.destroy(),
                        $device::Event::Finished => state.done = true,
                        _ => {}
                    }
                }
            }

            impl Dispatch<$Source, ()> for State {
                fn event(state: &mut Self, source: &$Source, event: $source::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
                    match event {
                        $source::Event::Send { mime_type, fd } => {
                            if state.source.as_ref() == Some(source) {
                                state.core.send(&mime_type, fd);
                            }
                        }
                        $source::Event::Cancelled => {
                            if state.source.as_ref() == Some(source) {
                                state.source = None;
                                state.core.served = None;
                            }
                            source.destroy();
                        }
                        _ => {}
                    }
                }
            }
        }
    };
}

data_control!(
    ext,
    [wayland_protocols::ext::data_control::v1::client],
    ext_data_control_manager_v1::ExtDataControlManagerV1,
    ext_data_control_device_v1::ExtDataControlDeviceV1,
    ext_data_control_offer_v1::ExtDataControlOfferV1,
    ext_data_control_source_v1::ExtDataControlSourceV1,
    1..=1
);

data_control!(
    wlr,
    [wayland_protocols_wlr::data_control::v1::client],
    zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
    zwlr_data_control_device_v1::ZwlrDataControlDeviceV1,
    zwlr_data_control_offer_v1::ZwlrDataControlOfferV1,
    zwlr_data_control_source_v1::ZwlrDataControlSourceV1,
    1..=2
);

/// Start recording the clipboard in the background, reconnecting when the
/// connection to the compositor is lost.
pub(crate) fn start(limits: Limits, exclude: Vec<regex::Regex>) {
    let (jobs, queue) = mpsc::sync_channel::<Job>(MAX_PENDING_JOBS);
    std::thread::Builder::new()
        .name("raffi-clipboard-record".into())
        .spawn(move || record(queue, limits, exclude))
        .ok();
    std::thread::Builder::new()
        .name("raffi-clipboard".into())
        .spawn(move || {
            let mut last_error = None;
            loop {
                let core = || {
                    Ok(Core {
                        store: Store::open_default()?,
                        jobs: jobs.clone(),
                        served: None,
                        persist: Persist::default(),
                    })
                };
                let result = match Connection::connect_to_env() {
                    Ok(conn) => match ext::run(&conn, core) {
                        Err(RunError::Unsupported) => wlr::run(&conn, core),
                        other => other,
                    },
                    Err(error) => Err(RunError::Failed(format!(
                        "cannot connect to the Wayland compositor: {error}"
                    ))),
                };
                let message = match result {
                    Ok(()) => continue,
                    Err(RunError::Unsupported) => {
                        eprintln!(
                            "raffi: the compositor does not support data-control; \
                             the clipboard history is not recorded"
                        );
                        return;
                    }
                    Err(RunError::Failed(message)) => message,
                };
                if last_error.as_ref() != Some(&message) {
                    eprintln!("raffi: clipboard recorder: {message}");
                    last_error = Some(message);
                }
                std::thread::sleep(RECONNECT_DELAY);
            }
        })
        .ok();
    eprintln!("raffi: recording the clipboard history");
}

/// Put the history entry `hash` on the clipboard. Called by the socket
/// server for the UI.
pub(crate) fn select(hash: &str) -> Result<(), String> {
    let sender = COMMANDS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clone()
        .ok_or_else(|| {
            "the Raffi server is not recording the clipboard; \
             set addons.clipboard.backend to raffi and restart it"
                .to_string()
        })?;
    let (reply, answer) = mpsc::channel();
    sender
        .send(Command::Select {
            hash: hash.to_string(),
            reply,
        })
        .map_err(|_| "the clipboard recorder stopped".to_string())?;
    answer
        .recv_timeout(SELECT_TIMEOUT)
        .map_err(|_| "the clipboard recorder did not answer".to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mimes(list: &[&str]) -> Vec<String> {
        list.iter().map(|mime| mime.to_string()).collect()
    }

    #[test]
    fn test_plan_reads_picks_kept_formats() {
        let plan = plan_reads(&mimes(&[
            "TEXT",
            "text/plain",
            "text/plain;charset=utf-8",
            "text/html",
            "text/_moz_htmlinfo",
            "chromium/x-source-url",
        ]))
        .unwrap();
        assert_eq!(
            plan,
            [
                (
                    "text/plain;charset=utf-8".to_string(),
                    "text/plain".to_string()
                ),
                ("text/html".to_string(), "text/html".to_string()),
            ]
        );

        // A browser "Copy Image" offers one picture in many encodings.
        let plan = plan_reads(&mimes(&[
            "text/html",
            "image/bmp",
            "image/jpeg",
            "image/png",
            "image/svg+xml",
        ]))
        .unwrap();
        let requested: Vec<&str> = plan.iter().map(|(mime, _)| mime.as_str()).collect();
        assert_eq!(requested, ["text/html", "image/png", "image/svg+xml"]);

        let plan = plan_reads(&mimes(&["image/x-portable-pixmap"])).unwrap();
        assert_eq!(plan[0].1, "image/x-portable-pixmap");
    }

    #[test]
    fn test_plan_reads_skips_secrets_own_and_unknown_selections() {
        assert_eq!(
            plan_reads(&mimes(&["text/plain", "x-kde-passwordManagerHint"])),
            None
        );
        assert_eq!(plan_reads(&mimes(&["text/plain", OWN_MIME])), None);
        assert_eq!(plan_reads(&mimes(&["application/x-something"])), None);
        assert_eq!(plan_reads(&[]), None);
    }

    #[test]
    fn test_unchecked_text_format_only_matters_with_exclusions() {
        let exclude = [regex::Regex::new("^secret:").unwrap()];
        let failed = mimes(&["image/png", "text/plain"]);
        assert_eq!(unchecked_text_format(&failed, &exclude), Some("text/plain"));
        assert_eq!(unchecked_text_format(&failed, &[]), None);
        assert_eq!(
            unchecked_text_format(&mimes(&["image/png"]), &exclude),
            None
        );
        assert_eq!(unchecked_text_format(&[], &exclude), None);
    }

    #[test]
    fn test_offered_mimes_and_served_data() {
        let formats = vec![
            ("image/png".to_string(), b"png".to_vec()),
            ("text/plain".to_string(), b"hi".to_vec()),
        ];
        let offered = offered_mimes(&formats);
        assert_eq!(
            offered,
            mimes(&[
                "image/png",
                "text/plain",
                "text/plain;charset=utf-8",
                "UTF8_STRING",
                "STRING",
                "TEXT",
                OWN_MIME,
            ])
        );
        assert_eq!(data_for(&formats, "UTF8_STRING"), Some(&b"hi"[..]));
        assert_eq!(data_for(&formats, "image/png"), Some(&b"png"[..]));
        assert_eq!(data_for(&formats, OWN_MIME), None);
    }

    #[test]
    fn test_persist_restores_only_the_cleared_selection() {
        let mut persist = Persist::default();
        // Startup with an empty clipboard: nothing to restore.
        assert_eq!(persist.cleared(), None);

        // Recorded, then its app exits.
        let first = persist.selection(None);
        assert_eq!(persist.recorded(first, "a".into()), None);
        assert_eq!(persist.cleared(), Some("a".into()));

        // The app exits before its copy is recorded.
        let second = persist.selection(None);
        assert_eq!(persist.cleared(), None);
        assert_eq!(persist.recorded(second, "b".into()), Some("b".into()));

        // A late recording of an older copy changes nothing.
        let third = persist.selection(None);
        assert_eq!(persist.recorded(second, "b".into()), None);
        // A skipped copy, such as a password, is never replaced by an
        // older entry when cleared.
        assert_eq!(persist.cleared(), None);
        let _ = third;

        // Raffi's own selection holds the entry it serves.
        persist.selection(Some("c".into()));
        assert_eq!(persist.cleared(), Some("c".into()));

        // After a reconnection, a recording queued before it never matches.
        let mut reconnected = Persist::default();
        let fresh = reconnected.selection(None);
        assert_ne!(fresh, third);
        assert_eq!(reconnected.recorded(third, "old".into()), None);
        assert_eq!(reconnected.cleared(), None);
    }

    #[test]
    fn test_pipe_round_trip_and_cap() {
        let (read, write) = pipe().unwrap();
        let writer = std::thread::spawn(move || {
            write_pipe(
                write,
                &[7u8; 200_000],
                Instant::now() + Duration::from_secs(5),
            )
        });
        let data = read_pipe(read, 1 << 20, Instant::now() + Duration::from_secs(5)).unwrap();
        writer.join().unwrap().unwrap();
        assert_eq!(data.len(), 200_000);

        let (read, write) = pipe().unwrap();
        let writer = std::thread::spawn(move || {
            let _ = write_pipe(write, &[1u8; 100], Instant::now() + Duration::from_secs(5));
        });
        assert!(read_pipe(read, 10, Instant::now() + Duration::from_secs(5)).is_err());
        writer.join().unwrap();

        // A source that never writes nor closes times out.
        let (read, _write) = pipe().unwrap();
        assert!(read_pipe(read, 10, Instant::now() + Duration::from_millis(50)).is_err());
    }

    #[test]
    fn test_focused_app_from_each_compositor() {
        assert_eq!(
            focused_app_from(Compositor::Hyprland, br#"{"class":"firefox","title":"x"}"#),
            Some("firefox".into())
        );
        assert_eq!(focused_app_from(Compositor::Hyprland, b"{}"), None);
        assert_eq!(
            focused_app_from(Compositor::Niri, br#"{"app_id":"org.gnome.Nautilus"}"#),
            Some("org.gnome.Nautilus".into())
        );
        let tree = br#"{"focused":false,"nodes":[{"focused":false,"nodes":[
            {"focused":false,"app_id":"kitty"}],"floating_nodes":[
            {"focused":true,"app_id":null,"window_properties":{"class":"Gimp"}}]}]}"#;
        assert_eq!(
            focused_app_from(Compositor::Sway, tree),
            Some("Gimp".into())
        );
    }
}
