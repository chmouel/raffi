//! Native messaging host for the Vicinae browser extension.
//!
//! The browser starts `raffi` as the `com.vicinae.vicinae` host when the
//! extension loads. The host keeps the tab list the extension reports and
//! serves it to the launcher over a Unix socket, one per running browser.
//! The launcher asks the host to focus or close a tab, and the host passes
//! that on to the extension.

use std::ffi::OsStr;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::fd::{AsFd, AsRawFd, RawFd};
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, TryLockError};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const HOST_NAME: &str = "com.vicinae.vicinae";
pub const CHROME_EXTENSION_ID: &str = "kcmipingpfbohfjckomimmahknoddnke";
pub const FIREFOX_EXTENSION_ID: &str = "firefox@vicinae.com";
pub const CHROME_STORE_URL: &str =
    "https://chromewebstore.google.com/detail/vicinae-integration/kcmipingpfbohfjckomimmahknoddnke";
pub const FIREFOX_STORE_URL: &str = "https://addons.mozilla.org/firefox/addon/vicinae/";

/// The extension may send up to 64 MiB; anything bigger is a broken stream.
const MAX_FRAME: u32 = 64 * 1024 * 1024;
const MAX_REQUEST: u64 = 64 * 1024;
const CLIENT_TIMEOUT: Duration = Duration::from_secs(2);
/// How long a launcher request waits for the browser to take a message.
const OUTPUT_WAIT: Duration = Duration::from_millis(500);
const MAX_CLIENTS: usize = 8;
/// Written into every manifest Raffi installs, to tell them apart from
/// manifests of other hosts.
const MANIFEST_DESCRIPTION: &str = "Raffi native messaging host for the Vicinae browser extension";

/// A browser tab as the extension reports it.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct Tab {
    pub id: i64,
    pub title: String,
    pub url: String,
    pub window_id: i64,
    pub active: bool,
    pub audible: bool,
    pub muted: bool,
    /// Milliseconds since the epoch.
    pub last_accessed: f64,
    pub pinned: bool,
}

/// The browser a host serves.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Browser {
    /// Short name, also the usual icon name and window class, such as
    /// `firefox` or `google-chrome`.
    pub id: String,
    pub name: String,
    /// The browser process, which owns its windows.
    pub pid: Option<u32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Listing {
    pub browser: Browser,
    pub tabs: Vec<Tab>,
}

/// A launcher request, one JSON line per connection.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "method", rename_all = "lowercase")]
pub enum Request {
    List,
    Focus {
        #[serde(rename = "tabId")]
        tab_id: i64,
    },
    Close {
        #[serde(rename = "tabId")]
        tab_id: i64,
    },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Response {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub listing: Option<Listing>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Whether the browser started this process as a native messaging host.
/// Chrome passes the extension origin first; Firefox passes the manifest
/// path, then the extension ID.
pub fn is_native_host_invocation(args: &[String]) -> bool {
    args.get(1)
        .is_some_and(|arg| arg.starts_with("chrome-extension://"))
        || args.get(2).is_some_and(|arg| arg == FIREFOX_EXTENSION_ID)
}

/// Read one native messaging frame: a native-endian u32 length, then JSON.
/// Returns None at end of input.
pub fn read_frame(reader: &mut impl Read) -> io::Result<Option<Vec<u8>>> {
    let mut size = [0u8; 4];
    match reader.read_exact(&mut size) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error),
    }
    let size = u32::from_ne_bytes(size);
    if size == 0 || size > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid message size {size}"),
        ));
    }
    let mut body = vec![0u8; size as usize];
    reader.read_exact(&mut body)?;
    Ok(Some(body))
}

/// Write one frame in a single write, which a pipe takes whole when it is
/// at most `PIPE_BUF` bytes.
pub fn write_frame(writer: &mut impl Write, body: &[u8]) -> io::Result<()> {
    let size = u32::try_from(body.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "message too large"))?;
    let mut frame = Vec::with_capacity(4 + body.len());
    frame.extend_from_slice(&size.to_ne_bytes());
    frame.extend_from_slice(body);
    writer.write_all(&frame)?;
    writer.flush()
}

/// Whether `fd` can take a write before `deadline`.
fn writable_before(fd: RawFd, deadline: Instant) -> bool {
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let mut poll = libc::pollfd {
            fd,
            events: libc::POLLOUT,
            revents: 0,
        };
        let millis =
            libc::c_int::try_from(left.as_micros().div_ceil(1000)).unwrap_or(libc::c_int::MAX);
        // SAFETY: one valid pollfd, for the duration of the call.
        match unsafe { libc::poll(&mut poll, 1, millis) } {
            1 => return poll.revents & libc::POLLOUT != 0,
            -1 if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted => {}
            _ => return false,
        }
    }
}

/// The host state shared by the extension reader and the socket clients.
pub struct Host<W> {
    listing: Mutex<Listing>,
    out: Mutex<W>,
    /// The descriptor behind `out`, polled before launcher messages so
    /// one is never left blocked, and delivered late, when the browser
    /// stops reading.
    out_fd: Option<RawFd>,
}

impl<W: Write> Host<W> {
    pub fn new(browser: Browser, out: W) -> Self {
        Self {
            listing: Mutex::new(Listing {
                browser,
                tabs: Vec::new(),
            }),
            out: Mutex::new(out),
            out_fd: None,
        }
    }

    /// Wait for `fd`, which `out` writes to, to have room before sending
    /// launcher messages.
    pub fn polling(mut self, fd: RawFd) -> Self {
        self.out_fd = Some(fd);
        self
    }

    fn send(&self, message: &Value) -> io::Result<()> {
        let body = serde_json::to_vec(message)?;
        let mut out = self
            .out
            .lock()
            .map_err(|_| io::Error::other("output lock poisoned"))?;
        write_frame(&mut *out, &body)
    }

    /// Send on behalf of a launcher. When an earlier message is still stuck
    /// because the browser stopped reading, give up instead of queueing
    /// behind it. Nothing is sent once `expires` has passed.
    fn send_for_client(&self, message: &Value, expires: Option<Instant>) -> io::Result<()> {
        let body = serde_json::to_vec(message)?;
        let expired = || expires.is_some_and(|expires| Instant::now() >= expires);
        let give_up = Instant::now() + OUTPUT_WAIT;
        loop {
            match self.out.try_lock() {
                Ok(_) if expired() => return Err(timed_out("the request expired")),
                Ok(mut out) => {
                    let expiring = expires.filter(|expires| *expires <= give_up);
                    let deadline = expiring.unwrap_or(give_up);
                    if self.out_fd.is_some_and(|fd| !writable_before(fd, deadline)) {
                        return Err(if expiring.is_some() {
                            timed_out("the request expired")
                        } else {
                            io::Error::new(
                                io::ErrorKind::WouldBlock,
                                "the browser is not reading messages",
                            )
                        });
                    }
                    return write_frame(&mut *out, &body);
                }
                Err(TryLockError::WouldBlock) if expired() => {
                    return Err(timed_out("the request expired"))
                }
                Err(TryLockError::WouldBlock) if Instant::now() < give_up => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(TryLockError::WouldBlock) => {
                    return Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "the browser is not reading messages",
                    ))
                }
                Err(TryLockError::Poisoned(_)) => {
                    return Err(io::Error::other("output lock poisoned"))
                }
            }
        }
    }

    /// Handle a message from the extension.
    pub fn handle_extension_message(&self, raw: &[u8]) -> io::Result<()> {
        let message: Value = serde_json::from_slice(raw)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let Some(method) = message.get("method").and_then(Value::as_str) else {
            // A response to a request; the host sends none.
            return Ok(());
        };
        let id = message.get("id").filter(|id| !id.is_null());
        match method {
            "browser/init" => match id {
                Some(id) => self.send(&json!({ "id": id, "result": {} })),
                None => Ok(()),
            },
            "browser/tabs-changed" => {
                let data = message.get("data").cloned().unwrap_or(Value::Null);
                match serde_json::from_value::<Vec<Tab>>(data) {
                    Ok(tabs) => {
                        if let Ok(mut listing) = self.listing.lock() {
                            listing.tabs = tabs;
                        }
                    }
                    Err(error) => eprintln!("raffi browser host: invalid tab list: {error}"),
                }
                Ok(())
            }
            _ => match id {
                Some(id) => self.send(&json!({ "id": id, "error": "unsupported method" })),
                None => Ok(()),
            },
        }
    }

    /// Answer a launcher request.
    pub fn handle_request(&self, request: Request) -> Response {
        self.handle_request_until(request, None)
    }

    /// Answer a launcher request, without acting on it after `expires`.
    pub fn handle_request_until(&self, request: Request, expires: Option<Instant>) -> Response {
        let error = |message: String| Response {
            listing: None,
            error: Some(message),
        };
        let Ok(listing) = self.listing.lock() else {
            return error("host state is unavailable".to_string());
        };
        let (method, tab_id) = match request {
            Request::List => {
                return Response {
                    listing: Some(listing.clone()),
                    error: None,
                }
            }
            Request::Focus { tab_id } => ("browser/focus-tab", tab_id),
            Request::Close { tab_id } => ("browser/close-tab", tab_id),
        };
        if !listing.tabs.iter().any(|tab| tab.id == tab_id) {
            return error("the tab is no longer open".to_string());
        }
        drop(listing);
        if let Err(err) = self.send_for_client(
            &json!({ "method": method, "data": { "tabId": tab_id } }),
            expires,
        ) {
            return error(format!("cannot reach the browser: {err}"));
        }
        if method == "browser/close-tab" {
            // Gone right away, so a list fetched before the extension's next
            // update does not show it.
            if let Ok(mut listing) = self.listing.lock() {
                listing.tabs.retain(|tab| tab.id != tab_id);
            }
        }
        Response::default()
    }

    /// Serve one launcher connection: a request line, then a response line.
    pub fn serve_client(&self, stream: UnixStream) -> io::Result<()> {
        stream.set_read_timeout(Some(CLIENT_TIMEOUT))?;
        stream.set_write_timeout(Some(CLIENT_TIMEOUT))?;
        let mut line = String::new();
        BufReader::new((&stream).take(MAX_REQUEST)).read_line(&mut line)?;
        let parsed = serde_json::from_str::<Value>(line.trim()).and_then(|value| {
            let expires = value.get("expires").and_then(Value::as_u64);
            serde_json::from_value::<Request>(value).map(|request| (request, expires))
        });
        let error = |message: String| Response {
            listing: None,
            error: Some(message),
        };
        let response = match parsed {
            Ok((request, None)) => self.handle_request(request),
            // The launcher stops waiting at `expires`; acting later would
            // surprise it.
            Ok((request, Some(expires))) => match monotonic_millis() {
                Ok(now) if expires > now => self.handle_request_until(
                    request,
                    Some(Instant::now() + Duration::from_millis(expires - now)),
                ),
                Ok(_) => error("the request expired".to_string()),
                Err(clock) => error(format!("cannot read the clock: {clock}")),
            },
            Err(error) => Response {
                listing: None,
                error: Some(format!("invalid request: {error}")),
            },
        };
        let mut body = serde_json::to_vec(&response)?;
        body.push(b'\n');
        (&stream).write_all(&body)
    }
}

/// Milliseconds on the monotonic clock, which all processes on the machine
/// share and which changes to the wall clock do not move.
fn monotonic_millis() -> io::Result<u64> {
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `now` is a valid timespec for the call to fill in.
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(now.tv_sec as u64 * 1000 + now.tv_nsec as u64 / 1_000_000)
}

struct ClientSlot(Arc<AtomicUsize>);

impl Drop for ClientSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Accept launcher connections, each on its own thread, at most `max` at
/// a time. Connections past the limit are closed right away.
pub fn serve_clients<W: Write + Send + 'static>(
    listener: UnixListener,
    host: Arc<Host<W>>,
    max: usize,
) {
    let active = Arc::new(AtomicUsize::new(0));
    for stream in listener.incoming().flatten() {
        let busy = active.fetch_add(1, Ordering::SeqCst) >= max;
        let slot = ClientSlot(Arc::clone(&active));
        if busy {
            eprintln!("raffi browser host: too many clients, dropping one");
            continue;
        }
        let host = Arc::clone(&host);
        let spawned = thread::Builder::new()
            .name("raffi-browser-client".to_string())
            .spawn(move || {
                let _slot = slot;
                if let Err(error) = host.serve_client(stream) {
                    eprintln!("raffi browser host: client error: {error}");
                }
            });
        if let Err(error) = spawned {
            eprintln!("raffi browser host: cannot serve a client: {error}");
        }
    }
}

fn current_uid() -> io::Result<u32> {
    fs::metadata("/proc/self").map(|meta| meta.uid())
}

/// Where the hosts put their sockets: `$XDG_RUNTIME_DIR/raffi/browsers`, or
/// `/tmp/raffi-<uid>/browsers` without a runtime directory. Creates the
/// directories, and refuses ones that are not private to this user.
pub fn socket_dir() -> io::Result<PathBuf> {
    let uid = current_uid()?;
    let base = match std::env::var_os("XDG_RUNTIME_DIR").filter(|dir| !dir.is_empty()) {
        Some(dir) => PathBuf::from(dir).join("raffi"),
        None => std::env::temp_dir().join(format!("raffi-{uid}")),
    };
    private_dir(&base, uid)?;
    let dir = base.join("browsers");
    private_dir(&dir, uid)?;
    Ok(dir)
}

/// Create `dir` readable by its owner only, or check that the existing one
/// is a real directory owned by `uid`. Symbolic links are refused.
fn private_dir(dir: &Path, uid: u32) -> io::Result<()> {
    match fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    let meta = fs::symlink_metadata(dir)?;
    if !meta.file_type().is_dir() || meta.uid() != uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} is not a directory owned by you", dir.display()),
        ));
    }
    if meta.mode() & 0o077 != 0 {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Name a browser from its executable path.
pub fn classify_browser(exe: &Path) -> Option<(&'static str, &'static str)> {
    let name = exe.file_name()?.to_str()?.to_lowercase();
    let path = exe.to_string_lossy().to_lowercase();
    Some(match name.as_str() {
        "firefox" | "firefox-bin" | "firefox-esr" => ("firefox", "Firefox"),
        "librewolf" => ("librewolf", "LibreWolf"),
        "zen" | "zen-bin" => ("zen", "Zen"),
        "floorp" => ("floorp", "Floorp"),
        "waterfox" => ("waterfox", "Waterfox"),
        "brave" | "brave-browser" => ("brave-browser", "Brave"),
        "chrome" if path.contains("chrome-beta") => ("google-chrome-beta", "Google Chrome Beta"),
        "chrome" if path.contains("chrome-unstable") => {
            ("google-chrome-unstable", "Google Chrome Dev")
        }
        "chrome" if path.contains("chromium") => ("chromium", "Chromium"),
        "chrome" | "google-chrome" | "google-chrome-stable" => ("google-chrome", "Google Chrome"),
        "chromium" | "chromium-browser" => ("chromium", "Chromium"),
        "vivaldi" | "vivaldi-bin" => ("vivaldi-stable", "Vivaldi"),
        "msedge" | "microsoft-edge" => ("microsoft-edge", "Microsoft Edge"),
        "opera" => ("opera", "Opera"),
        "thorium" | "thorium-browser" => ("thorium-browser", "Thorium"),
        "helium" => ("helium", "Helium"),
        _ => return None,
    })
}

fn parent_of(pid: u32) -> Option<u32> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name is in parentheses and may hold spaces.
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(1)?.parse().ok()
}

/// Find the browser among the ancestors of this process, or guess it from
/// how it was started.
fn detect_browser(args: &[String]) -> Browser {
    let mut pid = Some(std::os::unix::process::parent_id());
    for _ in 0..4 {
        let Some(current) = pid.filter(|&pid| pid > 1) else {
            break;
        };
        if let Some((id, name)) = fs::read_link(format!("/proc/{current}/exe"))
            .ok()
            .as_deref()
            .and_then(classify_browser)
        {
            return Browser {
                id: id.to_string(),
                name: name.to_string(),
                pid: Some(current),
            };
        }
        pid = parent_of(current);
    }
    let (id, name) = if args.get(2).is_some_and(|arg| arg == FIREFOX_EXTENSION_ID) {
        ("firefox", "Firefox")
    } else {
        ("chromium", "Chromium")
    };
    Browser {
        id: id.to_string(),
        name: name.to_string(),
        pid: None,
    }
}

/// Run as the native messaging host until the browser closes stdin. Returns
/// the exit code.
pub fn run_native_host(args: &[String]) -> i32 {
    let browser = detect_browser(args);
    let dir = match socket_dir() {
        Ok(dir) => dir,
        Err(error) => {
            eprintln!("raffi browser host: no socket directory: {error}");
            return 1;
        }
    };
    let socket = dir.join(format!("{}.sock", std::process::id()));
    let _ = fs::remove_file(&socket);
    let listener = match UnixListener::bind(&socket) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!(
                "raffi browser host: cannot listen on {}: {error}",
                socket.display()
            );
            return 1;
        }
    };
    let _ = fs::set_permissions(&socket, fs::Permissions::from_mode(0o600));
    // Unbuffered, so each frame is one write the poll in send_for_client
    // can vouch for.
    let out = match io::stdout().as_fd().try_clone_to_owned() {
        Ok(out) => fs::File::from(out),
        Err(error) => {
            eprintln!("raffi browser host: cannot use the output: {error}");
            let _ = fs::remove_file(&socket);
            return 1;
        }
    };
    let out_fd = out.as_raw_fd();
    let host = Arc::new(Host::new(browser, out).polling(out_fd));

    let server = Arc::clone(&host);
    if let Err(error) = thread::Builder::new()
        .name("raffi-browser-server".to_string())
        .spawn(move || serve_clients(listener, server, MAX_CLIENTS))
    {
        eprintln!("raffi browser host: cannot start the socket server: {error}");
        let _ = fs::remove_file(&socket);
        return 1;
    }

    let mut stdin = io::stdin().lock();
    let code = loop {
        match read_frame(&mut stdin) {
            Ok(Some(body)) => {
                if let Err(error) = host.handle_extension_message(&body) {
                    eprintln!("raffi browser host: {error}");
                }
            }
            Ok(None) => break 0,
            Err(error) => {
                eprintln!("raffi browser host: {error}");
                break 1;
            }
        }
    };
    let _ = fs::remove_file(&socket);
    code
}

fn timed_out(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, message.to_string())
}

fn time_left(deadline: Instant, message: &str) -> io::Result<Duration> {
    match deadline.checked_duration_since(Instant::now()) {
        Some(left) if !left.is_zero() => Ok(left),
        _ => Err(timed_out(message)),
    }
}

/// Connect, giving up at `deadline`. A blocking connect to a host whose
/// backlog is full waits forever, and the standard library has no connect
/// timeout for Unix sockets, so this retries a non-blocking one.
fn connect_before(socket: &Path, deadline: Instant) -> io::Result<UnixStream> {
    use std::os::fd::FromRawFd;
    use std::os::unix::ffi::OsStrExt;

    let path = socket.as_os_str().as_bytes();
    // SAFETY: sockaddr_un is plain data; all zeroes is a valid value.
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if path.len() >= addr.sun_path.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "socket path too long",
        ));
    }
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (slot, byte) in addr.sun_path.iter_mut().zip(path) {
        *slot = *byte as libc::c_char;
    }
    let len = (std::mem::size_of::<libc::sa_family_t>() + path.len() + 1) as libc::socklen_t;
    // SAFETY: plain system call; the result is checked below.
    let fd = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            0,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fd is a new socket nothing else owns; the stream closes it.
    let stream = unsafe { UnixStream::from_raw_fd(fd) };
    loop {
        // SAFETY: addr is a valid sockaddr_un and len covers its used part.
        let result =
            unsafe { libc::connect(fd, std::ptr::addr_of!(addr).cast::<libc::sockaddr>(), len) };
        if result == 0 {
            break;
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::EISCONN) => break,
            Some(libc::EAGAIN | libc::EINPROGRESS | libc::EALREADY | libc::EINTR) => {
                time_left(deadline, "the browser host is not accepting connections")?;
                thread::sleep(Duration::from_millis(10));
            }
            _ => return Err(error),
        }
    }
    stream.set_nonblocking(false)?;
    Ok(stream)
}

/// Send one request and read the answer, all before `deadline`. The host
/// drops requests that reach it after the deadline, so a request that timed
/// out here is not carried out later.
fn request_before(socket: &Path, request: &Request, deadline: Instant) -> io::Result<Response> {
    let mut stream = connect_before(socket, deadline)?;
    let mut message = serde_json::to_value(request)?;
    let left = time_left(deadline, "the browser host did not answer in time")?;
    message["expires"] = json!(monotonic_millis()? + left.as_millis() as u64);
    let mut body = serde_json::to_vec(&message)?;
    body.push(b'\n');
    stream.set_write_timeout(Some(left))?;
    stream.write_all(&body)?;

    let unanswered =
        "the browser host did not answer in time; the request may still be carried out";
    let mut answer = Vec::new();
    let mut chunk = [0u8; 8192];
    while !answer.ends_with(b"\n") {
        stream.set_read_timeout(Some(time_left(deadline, unanswered)?))?;
        let read = match stream.read(&mut chunk) {
            Ok(read) => read,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                return Err(timed_out(unanswered))
            }
            Err(error) => return Err(error),
        };
        if read == 0 {
            break;
        }
        answer.extend_from_slice(&chunk[..read]);
        if answer.len() > MAX_FRAME as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "answer too large",
            ));
        }
    }
    serde_json::from_slice(&answer)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

/// Ask the host behind `socket` to focus or close a tab.
pub fn send_request(socket: &Path, req: &Request) -> Result<(), String> {
    let response = request_before(socket, req, Instant::now() + CLIENT_TIMEOUT).map_err(
        |error| match error.kind() {
            io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => {
                "the browser is no longer connected".to_string()
            }
            _ => format!("browser host: {error}"),
        },
    )?;
    match response.error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// The tabs of every connected browser, with the socket of its host. Sockets
/// nobody listens on any more are removed.
pub fn list_browsers(dir: &Path) -> Vec<(PathBuf, Listing)> {
    list_browsers_within(dir, CLIENT_TIMEOUT)
}

/// Ask every host at once and wait at most `timeout` overall, so one stuck
/// host does not hide the others.
fn list_browsers_within(dir: &Path, timeout: Duration) -> Vec<(PathBuf, Listing)> {
    let (Ok(uid), Ok(entries)) = (current_uid(), fs::read_dir(dir)) else {
        return Vec::new();
    };
    let mut sockets: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension().is_some_and(|ext| ext == "sock")
                && fs::symlink_metadata(path)
                    .is_ok_and(|meta| meta.file_type().is_socket() && meta.uid() == uid)
        })
        .collect();
    sockets.sort();
    let deadline = Instant::now() + timeout;
    // Every request ends by the deadline, so the scope does too.
    let answers: Vec<io::Result<Response>> = thread::scope(|scope| {
        let handles: Vec<_> = sockets
            .iter()
            .map(|socket| scope.spawn(move || request_before(socket, &Request::List, deadline)))
            .collect();
        handles
            .into_iter()
            .map(|handle| {
                handle
                    .join()
                    .unwrap_or_else(|_| Err(io::Error::other("request thread panicked")))
            })
            .collect()
    });
    sockets
        .into_iter()
        .zip(answers)
        .filter_map(|(socket, answer)| match answer {
            Ok(Response {
                listing: Some(listing),
                ..
            }) => Some((socket, listing)),
            Ok(_) => None,
            Err(error) => {
                if error.kind() == io::ErrorKind::ConnectionRefused {
                    let _ = fs::remove_file(&socket);
                }
                crate::debug_log!("browser tabs: {}: {error}", socket.display());
                None
            }
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ManifestKind {
    Chromium,
    Gecko,
}

/// Where a browser looks for native messaging manifests, if it is installed.
struct ManifestTarget {
    /// The browser's own directory; nothing is written without it.
    probe: PathBuf,
    dir: PathBuf,
    kind: ManifestKind,
}

fn manifest_targets(home: &Path, config: &Path) -> Vec<ManifestTarget> {
    const CHROMIUM_DIRS: [&str; 10] = [
        "google-chrome",
        "google-chrome-beta",
        "google-chrome-unstable",
        "chromium",
        "thorium",
        "BraveSoftware/Brave-Browser",
        "microsoft-edge",
        "vivaldi",
        "opera",
        "net.imput.helium",
    ];
    let mut targets: Vec<ManifestTarget> = CHROMIUM_DIRS
        .iter()
        .map(|dir| ManifestTarget {
            probe: config.join(dir),
            dir: config.join(dir).join("NativeMessagingHosts"),
            kind: ManifestKind::Chromium,
        })
        .collect();
    let mozilla = home.join(".mozilla").join("native-messaging-hosts");
    let gecko = [
        (home.join(".mozilla"), mozilla.clone()),
        (
            config.join("mozilla"),
            config.join("mozilla").join("native-messaging-hosts"),
        ),
        (home.join(".waterfox"), mozilla.clone()),
        (home.join(".floorp"), mozilla.clone()),
        (home.join(".zen"), mozilla.clone()),
        (
            home.join(".zen"),
            home.join(".zen").join("native-messaging-hosts"),
        ),
        (
            home.join(".librewolf"),
            home.join(".librewolf").join("native-messaging-hosts"),
        ),
    ];
    targets.extend(gecko.into_iter().map(|(probe, dir)| ManifestTarget {
        probe,
        dir,
        kind: ManifestKind::Gecko,
    }));
    targets
}

fn manifest_json(kind: ManifestKind, exe: &Path) -> String {
    let mut manifest = json!({
        "name": HOST_NAME,
        "description": MANIFEST_DESCRIPTION,
        "path": exe.to_string_lossy(),
        "type": "stdio",
    });
    match kind {
        ManifestKind::Chromium => {
            manifest["allowed_origins"] =
                json!([format!("chrome-extension://{CHROME_EXTENSION_ID}/")]);
        }
        ManifestKind::Gecko => manifest["allowed_extensions"] = json!([FIREFOX_EXTENSION_ID]),
    }
    let mut text = serde_json::to_string_pretty(&manifest).unwrap_or_default();
    text.push('\n');
    text
}

/// Who an existing manifest belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Owner {
    /// Installed by Raffi, starting this program.
    Raffi(PathBuf),
    Vicinae,
    /// Anything else, described for the user.
    Other(String),
}

/// The owner of the manifest at `path`, or None when there is none.
fn manifest_owner(path: &Path) -> Option<Owner> {
    fs::symlink_metadata(path).ok()?;
    let manifest = fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok());
    let Some(manifest) = manifest else {
        return Some(Owner::Other("it cannot be read as a manifest".to_string()));
    };
    let program = manifest
        .get("path")
        .and_then(Value::as_str)
        .map(PathBuf::from);
    let ours = manifest.get("description").and_then(Value::as_str) == Some(MANIFEST_DESCRIPTION);
    Some(match program {
        Some(program) if ours => Owner::Raffi(program),
        Some(program) if is_vicinae(&program) => Owner::Vicinae,
        Some(program) => Owner::Other(format!("it starts {}", program.display())),
        None => Owner::Other("it names no program".to_string()),
    })
}

fn is_vicinae(program: &Path) -> bool {
    program
        .file_name()
        .is_some_and(|name| name.to_string_lossy().contains("vicinae"))
}

/// Write through a temporary file and a rename, so the browser never reads
/// half a manifest. A symbolic link at `path` is replaced, not followed.
fn write_manifest(dir: &Path, path: &Path, content: &str) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    let temp = dir.join(format!(".{HOST_NAME}.json.{}.tmp", std::process::id()));
    let result = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .open(&temp)
        .and_then(|mut file| {
            file.write_all(content.as_bytes())?;
            file.sync_all()
        })
        .and_then(|()| fs::rename(&temp, path));
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestOutcome {
    Written,
    Unchanged,
    /// Belongs to another host and was left alone; says why.
    Kept(String),
    Removed,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestReport {
    pub path: PathBuf,
    pub outcome: ManifestOutcome,
}

/// Point the manifest of every installed browser at `exe`. A manifest Raffi
/// did not install, or a symbolic link, is kept unless `force` is set.
pub fn install_manifests(
    home: &Path,
    config: &Path,
    exe: &Path,
    force: bool,
) -> Vec<ManifestReport> {
    let mut reports: Vec<ManifestReport> = Vec::new();
    for target in manifest_targets(home, config) {
        let path = target.dir.join(format!("{HOST_NAME}.json"));
        if !target.probe.is_dir() || reports.iter().any(|report| report.path == path) {
            continue;
        }
        let content = manifest_json(target.kind, exe);
        let is_link = fs::symlink_metadata(&path).is_ok_and(|meta| meta.file_type().is_symlink());
        let keep = match manifest_owner(&path) {
            _ if force => None,
            _ if is_link => Some("it is a symbolic link".to_string()),
            None | Some(Owner::Raffi(_)) => None,
            Some(Owner::Vicinae) => Some("it starts Vicinae".to_string()),
            Some(Owner::Other(reason)) => Some(reason),
        };
        let outcome =
            if !is_link && fs::read_to_string(&path).is_ok_and(|existing| existing == content) {
                ManifestOutcome::Unchanged
            } else if let Some(reason) = keep {
                ManifestOutcome::Kept(reason)
            } else {
                match write_manifest(&target.dir, &path, &content) {
                    Ok(()) => ManifestOutcome::Written,
                    Err(error) => ManifestOutcome::Failed(error.to_string()),
                }
            };
        reports.push(ManifestReport { path, outcome });
    }
    reports
}

/// Remove the manifests Raffi installed.
pub fn uninstall_manifests(home: &Path, config: &Path) -> Vec<ManifestReport> {
    let mut reports: Vec<ManifestReport> = Vec::new();
    for target in manifest_targets(home, config) {
        let path = target.dir.join(format!("{HOST_NAME}.json"));
        if reports.iter().any(|report| report.path == path)
            || !matches!(manifest_owner(&path), Some(Owner::Raffi(_)))
        {
            continue;
        }
        let outcome = match fs::remove_file(&path) {
            Ok(()) => ManifestOutcome::Removed,
            Err(error) => ManifestOutcome::Failed(error.to_string()),
        };
        reports.push(ManifestReport { path, outcome });
    }
    reports
}

/// How the browsers are set up to start a native host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostSetup {
    /// A manifest starts this Raffi binary.
    Raffi,
    /// Manifests start a Raffi binary that is gone, such as one moved since.
    MissingBinary(PathBuf),
    /// Manifests start Vicinae and none starts Raffi.
    Vicinae,
    /// Manifests belong to some other host; says why.
    Other(String),
    /// No manifest at all.
    None,
}

/// Find the manifests the browsers would read and what they start.
pub fn host_setup(home: &Path, config: &Path) -> HostSetup {
    let owners: Vec<Owner> = manifest_targets(home, config)
        .iter()
        .filter_map(|target| manifest_owner(&target.dir.join(format!("{HOST_NAME}.json"))))
        .collect();
    let raffi: Vec<&PathBuf> = owners
        .iter()
        .filter_map(|owner| match owner {
            Owner::Raffi(program) => Some(program),
            _ => None,
        })
        .collect();
    if raffi.iter().any(|program| program.exists()) {
        HostSetup::Raffi
    } else if let Some(program) = raffi.first() {
        HostSetup::MissingBinary(program.to_path_buf())
    } else if owners.contains(&Owner::Vicinae) {
        HostSetup::Vicinae
    } else if let Some(Owner::Other(reason)) = owners.first() {
        HostSetup::Other(reason.clone())
    } else {
        HostSetup::None
    }
}

/// The home and configuration directories the browsers use.
pub fn user_dirs() -> Result<(PathBuf, PathBuf)> {
    let home = PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?);
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"));
    Ok((home, config))
}

/// A browser on this machine that the Vicinae extension supports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledBrowser {
    pub name: &'static str,
    pub icon: &'static str,
    /// The executable found on PATH, used to open pages in this browser.
    pub program: PathBuf,
    pub store_url: &'static str,
    /// Some profile has the Vicinae extension.
    pub has_extension: bool,
}

/// Where a browser keeps its profiles.
enum ProfileRoot {
    Home(&'static str),
    Config(&'static str),
}

struct KnownBrowser {
    key: crate::BookmarkBrowser,
    name: &'static str,
    icon: &'static str,
    programs: &'static [&'static str],
    roots: &'static [ProfileRoot],
    kind: ManifestKind,
}

const KNOWN_BROWSERS: [KnownBrowser; 14] = {
    use crate::BookmarkBrowser as Key;
    use ManifestKind::{Chromium, Gecko};
    use ProfileRoot::{Config, Home};
    [
        KnownBrowser {
            key: Key::Firefox,
            name: "Firefox",
            icon: "firefox",
            programs: &["firefox", "firefox-esr"],
            roots: &[Home(".mozilla/firefox"), Config("mozilla/firefox")],
            kind: Gecko,
        },
        KnownBrowser {
            key: Key::Librewolf,
            name: "LibreWolf",
            icon: "librewolf",
            programs: &["librewolf"],
            roots: &[Home(".librewolf")],
            kind: Gecko,
        },
        KnownBrowser {
            key: Key::Zen,
            name: "Zen",
            icon: "zen-browser",
            programs: &["zen-browser", "zen"],
            roots: &[Home(".zen")],
            kind: Gecko,
        },
        KnownBrowser {
            key: Key::Floorp,
            name: "Floorp",
            icon: "floorp",
            programs: &["floorp"],
            roots: &[Home(".floorp")],
            kind: Gecko,
        },
        KnownBrowser {
            key: Key::Waterfox,
            name: "Waterfox",
            icon: "waterfox",
            programs: &["waterfox"],
            roots: &[Home(".waterfox")],
            kind: Gecko,
        },
        KnownBrowser {
            key: Key::Chrome,
            name: "Google Chrome",
            icon: "google-chrome",
            programs: &["google-chrome-stable", "google-chrome"],
            roots: &[Config("google-chrome")],
            kind: Chromium,
        },
        KnownBrowser {
            key: Key::ChromeBeta,
            name: "Google Chrome Beta",
            icon: "google-chrome-beta",
            programs: &["google-chrome-beta"],
            roots: &[Config("google-chrome-beta")],
            kind: Chromium,
        },
        KnownBrowser {
            key: Key::ChromeDev,
            name: "Google Chrome Dev",
            icon: "google-chrome-unstable",
            programs: &["google-chrome-unstable"],
            roots: &[Config("google-chrome-unstable")],
            kind: Chromium,
        },
        KnownBrowser {
            key: Key::Chromium,
            name: "Chromium",
            icon: "chromium",
            programs: &["chromium", "chromium-browser"],
            roots: &[Config("chromium")],
            kind: Chromium,
        },
        KnownBrowser {
            key: Key::Brave,
            name: "Brave",
            icon: "brave-browser",
            programs: &["brave", "brave-browser"],
            roots: &[Config("BraveSoftware/Brave-Browser")],
            kind: Chromium,
        },
        KnownBrowser {
            key: Key::Edge,
            name: "Microsoft Edge",
            icon: "microsoft-edge",
            programs: &["microsoft-edge-stable", "microsoft-edge"],
            roots: &[Config("microsoft-edge")],
            kind: Chromium,
        },
        KnownBrowser {
            key: Key::Vivaldi,
            name: "Vivaldi",
            icon: "vivaldi",
            programs: &["vivaldi-stable", "vivaldi"],
            roots: &[Config("vivaldi")],
            kind: Chromium,
        },
        KnownBrowser {
            key: Key::Opera,
            name: "Opera",
            icon: "opera",
            programs: &["opera"],
            roots: &[Config("opera")],
            kind: Chromium,
        },
        KnownBrowser {
            key: Key::Thorium,
            name: "Thorium",
            icon: "thorium-browser",
            programs: &["thorium-browser", "thorium"],
            roots: &[Config("thorium")],
            kind: Chromium,
        },
    ]
};

pub fn find_program(programs: &[&str], path: &OsStr) -> Option<PathBuf> {
    std::env::split_paths(path).find_map(|dir| {
        programs
            .iter()
            .map(|program| dir.join(program))
            .find(|candidate| candidate.is_file())
    })
}

/// Whether a profile directly under `root` has the Vicinae extension.
fn profiles_have_extension(root: &Path, kind: ManifestKind) -> bool {
    let Ok(entries) = fs::read_dir(root) else {
        return false;
    };
    entries.flatten().any(|profile| {
        let profile = profile.path();
        match kind {
            ManifestKind::Chromium => profile
                .join("Extensions")
                .join(CHROME_EXTENSION_ID)
                .is_dir(),
            ManifestKind::Gecko => {
                let extensions = profile.join("extensions");
                extensions
                    .join(format!("{FIREFOX_EXTENSION_ID}.xpi"))
                    .is_file()
                    || extensions.join(FIREFOX_EXTENSION_ID).is_dir()
            }
        }
    })
}

/// The supported browsers that have a profile directory and an executable
/// on `path`.
pub fn installed_browsers(home: &Path, config: &Path, path: &OsStr) -> Vec<InstalledBrowser> {
    KNOWN_BROWSERS
        .iter()
        .filter_map(|known| {
            let roots: Vec<PathBuf> = known
                .roots
                .iter()
                .map(|root| match root {
                    ProfileRoot::Home(dir) => home.join(dir),
                    ProfileRoot::Config(dir) => config.join(dir),
                })
                .filter(|root| root.is_dir())
                .collect();
            if roots.is_empty() {
                return None;
            }
            let program = find_program(known.programs, path)?;
            Some(InstalledBrowser {
                name: known.name,
                icon: known.icon,
                program,
                store_url: match known.kind {
                    ManifestKind::Chromium => CHROME_STORE_URL,
                    ManifestKind::Gecko => FIREFOX_STORE_URL,
                },
                has_extension: roots
                    .iter()
                    .any(|root| profiles_have_extension(root, known.kind)),
            })
        })
        .collect()
}

/// The favicon databases in the profiles of the browser named `id`, a
/// [`Browser::id`]; `true` marks Firefox-style ones.
pub fn profile_favicon_databases(id: &str, home: &Path, config: &Path) -> Vec<(PathBuf, bool)> {
    let Some(known) = KNOWN_BROWSERS
        .iter()
        .find(|known| known.icon == id || known.programs.contains(&id))
    else {
        return Vec::new();
    };
    let (file, gecko) = match known.kind {
        ManifestKind::Gecko => ("favicons.sqlite", true),
        ManifestKind::Chromium => ("Favicons", false),
    };
    known
        .roots
        .iter()
        .map(|root| match root {
            ProfileRoot::Home(dir) => home.join(dir),
            ProfileRoot::Config(dir) => config.join(dir),
        })
        .filter_map(|root| fs::read_dir(root).ok())
        .flat_map(|entries| entries.flatten())
        .map(|profile| profile.path().join(file))
        .filter(|database| database.is_file())
        .map(|database| (database, gecko))
        .collect()
}

fn known_browser(browser: crate::BookmarkBrowser) -> Option<&'static KnownBrowser> {
    KNOWN_BROWSERS.iter().find(|known| known.key == browser)
}

/// The display name and icon of `browser`, and whether it is a Firefox-style
/// browser.
pub fn browser_details(
    browser: crate::BookmarkBrowser,
) -> Option<(&'static str, &'static str, bool)> {
    known_browser(browser).map(|known| (known.name, known.icon, known.kind == ManifestKind::Gecko))
}

/// The window app IDs `browser` may use: its icon and program names.
pub fn browser_window_apps(browser: crate::BookmarkBrowser) -> Vec<String> {
    let Some(known) = known_browser(browser) else {
        return Vec::new();
    };
    let mut apps = vec![known.icon.to_string()];
    for program in known.programs {
        if !apps.iter().any(|app| app == program) {
            apps.push(program.to_string());
        }
    }
    apps
}

/// The executable of `browser` found on `path`.
pub fn browser_program(browser: crate::BookmarkBrowser, path: &OsStr) -> Option<PathBuf> {
    find_program(known_browser(browser)?.programs, path)
}

/// A browser profile that has bookmarks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserProfile {
    pub browser: crate::BookmarkBrowser,
    pub dir: PathBuf,
    /// The name the browser shows for it, else the directory name.
    pub name: String,
}

/// The profiles with bookmarks of `browser`, or of every supported browser.
pub fn bookmark_profiles(
    browser: Option<crate::BookmarkBrowser>,
    home: &Path,
    config: &Path,
) -> Vec<BrowserProfile> {
    let mut profiles: Vec<BrowserProfile> = Vec::new();
    for known in KNOWN_BROWSERS
        .iter()
        .filter(|known| browser.is_none_or(|browser| known.key == browser))
    {
        let file = match known.kind {
            ManifestKind::Gecko => "places.sqlite",
            ManifestKind::Chromium => "Bookmarks",
        };
        let mut found: Vec<BrowserProfile> = Vec::new();
        for root in known.roots.iter().map(|root| match root {
            ProfileRoot::Home(dir) => home.join(dir),
            ProfileRoot::Config(dir) => config.join(dir),
        }) {
            let names = match known.kind {
                ManifestKind::Gecko => gecko_profile_names(&root),
                ManifestKind::Chromium => chromium_profile_names(&root),
            };
            let entries = fs::read_dir(&root)
                .into_iter()
                .flatten()
                .flatten()
                .map(|entry| entry.path());
            // Opera keeps its only profile in the root itself.
            let own = (known.kind == ManifestKind::Chromium).then(|| root.clone());
            for dir in own.into_iter().chain(entries) {
                if !dir.join(file).is_file() || found.iter().any(|profile| profile.dir == dir) {
                    continue;
                }
                let dir_name = dir
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let name = names
                    .iter()
                    .find(|(path, _)| *path == dir)
                    .map(|(_, name)| name.clone())
                    .unwrap_or(dir_name);
                found.push(BrowserProfile {
                    browser: known.key,
                    dir,
                    name,
                });
            }
        }
        found.sort_by(|a, b| a.name.cmp(&b.name));
        profiles.extend(found);
    }
    profiles
}

/// Profile directories and names from Firefox's `profiles.ini`.
fn gecko_profile_names(root: &Path) -> Vec<(PathBuf, String)> {
    let Ok(ini) = fs::read_to_string(root.join("profiles.ini")) else {
        return Vec::new();
    };
    let mut names = Vec::new();
    let mut section: (Option<String>, Option<String>, bool) = (None, None, true);
    let mut flush = |section: &mut (Option<String>, Option<String>, bool)| {
        if let (Some(name), Some(path)) = (section.0.take(), section.1.take()) {
            let dir = if section.2 {
                root.join(path)
            } else {
                PathBuf::from(path)
            };
            names.push((dir, name));
        }
        section.2 = true;
    };
    for line in ini.lines().map(str::trim) {
        if line.starts_with('[') {
            flush(&mut section);
        } else if let Some((key, value)) = line.split_once('=') {
            match key.trim() {
                "Name" => section.0 = Some(value.trim().to_string()),
                "Path" => section.1 = Some(value.trim().to_string()),
                "IsRelative" => section.2 = value.trim() != "0",
                _ => {}
            }
        }
    }
    flush(&mut section);
    names
}

/// Profile directories and names from a Chromium browser's `Local State`.
fn chromium_profile_names(root: &Path) -> Vec<(PathBuf, String)> {
    let Ok(text) = fs::read_to_string(root.join("Local State")) else {
        return Vec::new();
    };
    let Ok(state) = serde_json::from_str::<Value>(&text) else {
        return Vec::new();
    };
    let Some(cache) = state
        .pointer("/profile/info_cache")
        .and_then(Value::as_object)
    else {
        return Vec::new();
    };
    cache
        .iter()
        .filter_map(|(dir, info)| {
            let name = info.get("name")?.as_str()?;
            Some((root.join(dir), name.to_string()))
        })
        .collect()
}

/// Write the manifests for this Raffi binary, as `--install-browser-host`
/// does, and describe the result in one line.
pub fn install_current(force: bool) -> std::result::Result<String, String> {
    let (home, config) = user_dirs().map_err(|error| error.to_string())?;
    let exe = std::env::current_exe()
        .map_err(|error| format!("cannot find the raffi executable: {error}"))?;
    install_summary(&install_manifests(&home, &config, &exe, force))
}

fn install_summary(reports: &[ManifestReport]) -> std::result::Result<String, String> {
    let failures: Vec<String> = reports
        .iter()
        .filter_map(|report| match &report.outcome {
            ManifestOutcome::Failed(error) => Some(format!("{}: {error}", report.path.display())),
            _ => None,
        })
        .collect();
    if !failures.is_empty() {
        return Err(failures.join("; "));
    }
    let connected = reports
        .iter()
        .filter(|report| {
            matches!(
                report.outcome,
                ManifestOutcome::Written | ManifestOutcome::Unchanged
            )
        })
        .count();
    if connected > 0 {
        return Ok(format!("{connected} browser manifest(s) point to Raffi"));
    }
    match reports.iter().find_map(|report| match &report.outcome {
        ManifestOutcome::Kept(reason) => Some(reason),
        _ => None,
    }) {
        Some(reason) => Err(format!("left the existing manifests alone: {reason}")),
        None => Err("no supported browser found".to_string()),
    }
}

/// `raffi --install-browser-host` and `--uninstall-browser-host`.
pub fn manifest_command(uninstall: bool, force: bool) -> Result<()> {
    let (home, config) = user_dirs()?;
    let reports = if uninstall {
        uninstall_manifests(&home, &config)
    } else {
        let exe = std::env::current_exe().context("cannot find the raffi executable")?;
        install_manifests(&home, &config, &exe, force)
    };
    let mut failed = 0;
    for report in &reports {
        let path = report.path.display();
        match &report.outcome {
            ManifestOutcome::Written => println!("wrote {path}"),
            ManifestOutcome::Unchanged => println!("up to date {path}"),
            ManifestOutcome::Removed => println!("removed {path}"),
            ManifestOutcome::Kept(reason) => {
                eprintln!("skipped {path}: {reason} (add --force to replace it)")
            }
            ManifestOutcome::Failed(error) => {
                failed += 1;
                eprintln!("failed {path}: {error}");
            }
        }
    }
    if uninstall {
        if reports.is_empty() {
            println!("No Raffi browser host manifest found.");
        }
    } else if reports.is_empty() {
        println!(
            "No supported browser found in {} or {}.",
            home.display(),
            config.display()
        );
    } else {
        println!(
            "\nInstall the Vicinae browser extension if needed, then reload it:\n  Chrome, Brave and other Chromium browsers: {CHROME_STORE_URL}\n  Firefox: {FIREFOX_STORE_URL}"
        );
    }
    if failed > 0 {
        bail!("{failed} manifest(s) could not be updated");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_every_bookmark_browser_is_one_known_browser() {
        let names = [
            "firefox",
            "librewolf",
            "zen",
            "floorp",
            "waterfox",
            "chrome",
            "chrome-beta",
            "chrome-dev",
            "chromium",
            "brave",
            "edge",
            "vivaldi",
            "opera",
            "thorium",
        ];
        let keys: std::collections::HashSet<_> =
            KNOWN_BROWSERS.iter().map(|known| known.key).collect();
        assert_eq!(keys.len(), KNOWN_BROWSERS.len());
        assert_eq!(names.len(), KNOWN_BROWSERS.len());
        for name in names {
            let browser: crate::BookmarkBrowser = serde_yaml::from_str(name).unwrap();
            assert!(keys.contains(&browser), "{name}");
        }
    }
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time went backwards")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("raffi-{name}-{unique}"));
        fs::create_dir_all(&path).expect("failed to create temp dir");
        path
    }

    fn frame(value: &Value) -> Vec<u8> {
        let mut out = Vec::new();
        write_frame(&mut out, &serde_json::to_vec(value).unwrap()).unwrap();
        out
    }

    fn sent(host: &Host<Vec<u8>>) -> Vec<Value> {
        let out = host.out.lock().unwrap();
        let mut reader = Cursor::new(out.clone());
        let mut messages = Vec::new();
        while let Some(body) = read_frame(&mut reader).unwrap() {
            messages.push(serde_json::from_slice(&body).unwrap());
        }
        messages
    }

    fn host_with_tabs() -> Host<Vec<u8>> {
        let host = Host::new(Browser::default(), Vec::new());
        let tabs = json!({"method": "browser/tabs-changed", "data": [
            {"id": 7, "title": "Docs", "url": "https://docs.rs/", "windowId": 1,
             "active": true, "audible": false, "muted": false, "lastAccessed": 1.5,
             "pinned": false, "extra": "ignored"},
            {"id": 9, "title": "Music", "url": "https://music.example/", "audible": true},
        ]});
        host.handle_extension_message(&serde_json::to_vec(&tabs).unwrap())
            .unwrap();
        host
    }

    #[test]
    fn test_native_host_invocation_detects_both_browsers() {
        let args = |list: &[&str]| list.iter().map(|arg| arg.to_string()).collect::<Vec<_>>();
        assert!(is_native_host_invocation(&args(&[
            "raffi",
            "chrome-extension://kcmipingpfbohfjckomimmahknoddnke/",
            "--parent-window=0",
        ])));
        assert!(is_native_host_invocation(&args(&[
            "raffi",
            "/home/u/.mozilla/native-messaging-hosts/com.vicinae.vicinae.json",
            "firefox@vicinae.com",
        ])));
        assert!(!is_native_host_invocation(&args(&["raffi", "-i", "tabs"])));
    }

    #[test]
    fn test_frames_round_trip_and_reject_bad_sizes() {
        let mut data = frame(&json!({"a": 1}));
        data.extend(frame(&json!({"b": 2})));
        let mut reader = Cursor::new(data);
        assert_eq!(read_frame(&mut reader).unwrap().unwrap(), br#"{"a":1}"#);
        assert_eq!(read_frame(&mut reader).unwrap().unwrap(), br#"{"b":2}"#);
        assert!(read_frame(&mut reader).unwrap().is_none());

        let mut oversized = Cursor::new((MAX_FRAME + 1).to_ne_bytes().to_vec());
        assert!(read_frame(&mut oversized).is_err());
    }

    #[test]
    fn test_init_gets_a_response_with_the_same_id() {
        let host = Host::new(Browser::default(), Vec::new());
        let init = json!({"id": 0, "method": "browser/init",
            "data": {"id": "uuid", "name": "UA", "engine": "chromium"}});
        host.handle_extension_message(&serde_json::to_vec(&init).unwrap())
            .unwrap();
        assert_eq!(sent(&host), vec![json!({"id": 0, "result": {}})]);
    }

    #[test]
    fn test_tabs_changed_replaces_the_listing() {
        let host = host_with_tabs();
        let listing = host.handle_request(Request::List).listing.unwrap();
        assert_eq!(listing.tabs.len(), 2);
        assert_eq!(listing.tabs[0].title, "Docs");
        assert_eq!(listing.tabs[0].last_accessed, 1.5);
        assert!(listing.tabs[1].audible);
        assert!(sent(&host).is_empty());
    }

    #[test]
    fn test_focus_and_close_are_forwarded_to_the_extension() {
        let host = host_with_tabs();
        assert_eq!(
            host.handle_request(Request::Focus { tab_id: 7 }),
            Response::default()
        );
        assert_eq!(
            host.handle_request(Request::Close { tab_id: 9 }),
            Response::default()
        );
        assert_eq!(
            sent(&host),
            vec![
                json!({"method": "browser/focus-tab", "data": {"tabId": 7}}),
                json!({"method": "browser/close-tab", "data": {"tabId": 9}}),
            ]
        );
        let listing = host.handle_request(Request::List).listing.unwrap();
        assert_eq!(
            listing.tabs.len(),
            1,
            "a closed tab leaves the list at once"
        );
        assert!(host
            .handle_request(Request::Focus { tab_id: 9 })
            .error
            .is_some());
    }

    #[test]
    fn test_socket_clients_list_and_focus_tabs() {
        let dir = temp_dir("browser-host-socket");
        let socket = dir.join("1.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let host = Arc::new(host_with_tabs());
        let server = Arc::clone(&host);
        thread::spawn(move || serve_clients(listener, server, MAX_CLIENTS));
        let stale = dir.join("2.sock");
        drop(UnixListener::bind(&stale).unwrap());

        let browsers = list_browsers(&dir);
        assert_eq!(browsers.len(), 1);
        assert_eq!(browsers[0].0, socket);
        assert_eq!(browsers[0].1.tabs.len(), 2);
        assert!(!stale.exists(), "a socket nobody listens on is removed");

        send_request(&socket, &Request::Focus { tab_id: 7 }).unwrap();
        assert!(send_request(&socket, &Request::Close { tab_id: 42 }).is_err());
        assert_eq!(sent(&host).len(), 1);

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn test_a_silent_host_does_not_hide_the_others() {
        let dir = temp_dir("browser-host-silent");
        // Accepted by the kernel, never answered.
        let _silent = UnixListener::bind(dir.join("1.sock")).unwrap();
        let listener = UnixListener::bind(dir.join("2.sock")).unwrap();
        let host = Arc::new(host_with_tabs());
        thread::spawn(move || serve_clients(listener, host, MAX_CLIENTS));

        let started = Instant::now();
        let browsers = list_browsers_within(&dir, Duration::from_millis(300));
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(browsers.len(), 1);
        assert_eq!(browsers[0].0, dir.join("2.sock"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn test_requests_fail_fast_when_the_browser_stops_reading() {
        let host = host_with_tabs();
        let stuck = host.out.lock().unwrap();
        let started = Instant::now();
        let response = host.handle_request(Request::Close { tab_id: 7 });
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(response.error.unwrap().contains("not reading"));
        drop(stuck);
        assert_eq!(
            host.handle_request(Request::List)
                .listing
                .unwrap()
                .tabs
                .len(),
            2,
            "a tab whose close was not sent stays listed"
        );
    }

    #[test]
    fn test_clients_past_the_limit_are_turned_away() {
        let dir = temp_dir("browser-host-limit");
        let socket = dir.join("1.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let host = Arc::new(host_with_tabs());
        thread::spawn(move || serve_clients(listener, host, 2));

        let idle: Vec<UnixStream> = (0..2)
            .map(|_| UnixStream::connect(&socket).unwrap())
            .collect();
        thread::sleep(Duration::from_millis(100));
        let started = Instant::now();
        assert!(request_before(&socket, &Request::List, started + Duration::from_secs(5)).is_err());
        assert!(started.elapsed() < Duration::from_secs(1));

        drop(idle);
        let deadline = Instant::now() + Duration::from_secs(2);
        while list_browsers(&dir).is_empty() {
            assert!(Instant::now() < deadline, "freed slots serve clients again");
            thread::sleep(Duration::from_millis(20));
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn test_a_full_backlog_times_out_without_a_late_delivery() {
        use std::os::fd::AsRawFd;

        let dir = temp_dir("browser-host-backlog");
        let socket = dir.join("1.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        // SAFETY: shrink the backlog of a socket this test owns.
        assert_eq!(unsafe { libc::listen(listener.as_raw_fd(), 0) }, 0);
        let mut queued = Vec::new();
        while let Ok(stream) = connect_before(&socket, Instant::now() + Duration::from_millis(50)) {
            queued.push(stream);
            assert!(queued.len() < 64, "the backlog never filled");
        }

        let started = Instant::now();
        let close = Request::Close { tab_id: 7 };
        let error =
            request_before(&socket, &close, started + Duration::from_millis(200)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(1));
        for _ in 0..3 {
            let started = Instant::now();
            assert!(list_browsers_within(&dir, Duration::from_millis(100)).is_empty());
            assert!(started.elapsed() < Duration::from_secs(1));
        }

        drop(queued);
        listener.set_nonblocking(true).unwrap();
        while let Ok((stream, _)) = listener.accept() {
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_millis(50)))
                .unwrap();
            let mut received = String::new();
            let _ = (&stream).read_to_string(&mut received);
            assert!(
                !received.contains("close"),
                "a timed-out close was delivered"
            );
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn test_expired_requests_are_not_carried_out() {
        let host = host_with_tabs();
        let (client, server) = UnixStream::pair().unwrap();
        (&client)
            .write_all(b"{\"method\":\"close\",\"tabId\":7,\"expires\":1}\n")
            .unwrap();
        host.serve_client(server).unwrap();
        let mut answer = String::new();
        BufReader::new(&client).read_line(&mut answer).unwrap();
        assert!(answer.contains("expired"));
        assert!(sent(&host).is_empty());
    }

    #[test]
    fn test_a_request_expiring_while_output_is_busy_is_dropped() {
        let host = Arc::new(host_with_tabs());
        let busy = Arc::clone(&host);
        let (locked, wait_locked) = std::sync::mpsc::channel();
        let holder = thread::spawn(move || {
            let _out = busy.out.lock().unwrap();
            locked.send(()).unwrap();
            thread::sleep(Duration::from_millis(250));
        });
        wait_locked.recv().unwrap();
        let expires = Instant::now() + Duration::from_millis(100);
        let response = host.handle_request_until(Request::Close { tab_id: 7 }, Some(expires));
        holder.join().unwrap();
        assert!(response.error.unwrap().contains("expired"));
        assert!(sent(&host).is_empty());
        assert_eq!(
            host.handle_request(Request::List)
                .listing
                .unwrap()
                .tabs
                .len(),
            2
        );
    }

    #[test]
    fn test_a_close_is_not_delivered_late_when_the_browser_stops_reading() {
        let (mut reader, writer) = io::pipe().unwrap();
        let set_nonblocking = |fd: RawFd, on: bool| {
            // SAFETY: fcntl on descriptors owned by this test.
            unsafe {
                let flags = libc::fcntl(fd, libc::F_GETFL);
                let flags = if on {
                    flags | libc::O_NONBLOCK
                } else {
                    flags & !libc::O_NONBLOCK
                };
                libc::fcntl(fd, libc::F_SETFL, flags);
            }
        };
        let fd = writer.as_raw_fd();
        set_nonblocking(fd, true);
        let mut filler = &writer;
        while filler.write(&[0; 4096]).is_ok() {}
        set_nonblocking(fd, false);

        let host = Host::new(Browser::default(), writer).polling(fd);
        let tabs = json!({"method": "browser/tabs-changed", "data": [{"id": 7}]});
        host.handle_extension_message(&serde_json::to_vec(&tabs).unwrap())
            .unwrap();
        let expires = Instant::now() + Duration::from_millis(150);
        let response = host.handle_request_until(Request::Close { tab_id: 7 }, Some(expires));
        assert!(response.error.unwrap().contains("expired"));

        set_nonblocking(reader.as_raw_fd(), true);
        let mut drained = Vec::new();
        let _ = reader.read_to_end(&mut drained);
        assert!(!drained.windows(9).any(|window| window == b"close-tab"));
        let response = host.handle_request_until(
            Request::Close { tab_id: 7 },
            Some(Instant::now() + Duration::from_millis(150)),
        );
        assert_eq!(response.error, None);
        let mut sent = Vec::new();
        let _ = reader.read_to_end(&mut sent);
        assert!(sent.windows(9).any(|window| window == b"close-tab"));
    }

    #[test]
    fn test_private_dir_refuses_links_and_tightens_permissions() {
        let root = temp_dir("browser-host-private");
        let uid = current_uid().unwrap();
        let open = root.join("open");
        fs::create_dir(&open).unwrap();
        fs::set_permissions(&open, fs::Permissions::from_mode(0o755)).unwrap();
        private_dir(&open, uid).unwrap();
        assert_eq!(fs::metadata(&open).unwrap().mode() & 0o777, 0o700);

        let link = root.join("link");
        std::os::unix::fs::symlink(&open, &link).unwrap();
        assert!(private_dir(&link, uid).is_err());
        assert!(private_dir(&open, uid + 1).is_err());

        let fresh = root.join("fresh");
        private_dir(&fresh, uid).unwrap();
        assert_eq!(fs::metadata(&fresh).unwrap().mode() & 0o777, 0o700);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn test_classify_browser_names_common_executables() {
        let id = |path: &str| classify_browser(Path::new(path)).map(|(id, _)| id);
        assert_eq!(id("/usr/lib/firefox/firefox"), Some("firefox"));
        assert_eq!(id("/opt/google/chrome/chrome"), Some("google-chrome"));
        assert_eq!(id("/opt/brave.com/brave/brave"), Some("brave-browser"));
        assert_eq!(id("/usr/lib/chromium/chromium"), Some("chromium"));
        assert_eq!(id("/usr/bin/bash"), None);
    }

    #[test]
    fn test_host_setup_tells_missing_vicinae_and_moved_binary_apart() {
        let home = temp_dir("browser-host-setup");
        let config = home.join(".config");
        assert_eq!(host_setup(&home, &config), HostSetup::None);

        let brave = config.join("BraveSoftware/Brave-Browser/NativeMessagingHosts");
        fs::create_dir_all(&brave).unwrap();
        fs::write(
            brave.join("com.vicinae.vicinae.json"),
            r#"{"path": "/usr/libexec/vicinae/vicinae-browser-link"}"#,
        )
        .unwrap();
        assert_eq!(host_setup(&home, &config), HostSetup::Vicinae);

        let firefox = home.join(".mozilla/native-messaging-hosts");
        fs::create_dir_all(&firefox).unwrap();
        let gone = home.join("old/raffi");
        fs::write(
            firefox.join("com.vicinae.vicinae.json"),
            manifest_json(ManifestKind::Gecko, &gone),
        )
        .unwrap();
        assert_eq!(host_setup(&home, &config), HostSetup::MissingBinary(gone));

        let exe = home.join("bin/launcher");
        fs::create_dir_all(exe.parent().unwrap()).unwrap();
        fs::write(&exe, "").unwrap();
        fs::write(
            firefox.join("com.vicinae.vicinae.json"),
            manifest_json(ManifestKind::Gecko, &exe),
        )
        .unwrap();
        assert_eq!(host_setup(&home, &config), HostSetup::Raffi);

        fs::remove_file(firefox.join("com.vicinae.vicinae.json")).unwrap();
        fs::write(
            brave.join("com.vicinae.vicinae.json"),
            r#"{"path": "/opt/foreign/raffi-helper"}"#,
        )
        .unwrap();
        assert_eq!(
            host_setup(&home, &config),
            HostSetup::Other("it starts /opt/foreign/raffi-helper".to_string())
        );
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn test_installed_browsers_need_a_profile_dir_and_a_program_and_find_the_extension() {
        let home = temp_dir("browser-host-browsers");
        let config = home.join(".config");
        let bin = home.join("bin");
        fs::create_dir_all(&bin).unwrap();
        for program in ["firefox", "brave", "chromium"] {
            fs::write(bin.join(program), "").unwrap();
        }
        // Chromium has a program but no profile; Vivaldi a profile but no
        // program.
        fs::create_dir_all(home.join(".mozilla/firefox/abc.default/extensions")).unwrap();
        fs::create_dir_all(config.join("vivaldi/Default")).unwrap();
        let brave_profile = config.join("BraveSoftware/Brave-Browser/Profile 1");
        fs::create_dir_all(brave_profile.join("Extensions").join(CHROME_EXTENSION_ID)).unwrap();

        let browsers = installed_browsers(&home, &config, bin.as_os_str());
        let found: Vec<(&str, &str, bool)> = browsers
            .iter()
            .map(|browser| (browser.name, browser.store_url, browser.has_extension))
            .collect();
        assert_eq!(
            found,
            vec![
                ("Firefox", FIREFOX_STORE_URL, false),
                ("Brave", CHROME_STORE_URL, true),
            ]
        );
        assert_eq!(browsers[0].program, bin.join("firefox"));

        fs::write(
            home.join(".mozilla/firefox/abc.default/extensions/firefox@vicinae.com.xpi"),
            "",
        )
        .unwrap();
        assert!(installed_browsers(&home, &config, bin.as_os_str())[0].has_extension);
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn test_install_summary_reports_failures_kept_manifests_and_no_browser() {
        let report = |outcome| ManifestReport {
            path: PathBuf::from("/m.json"),
            outcome,
        };
        assert_eq!(
            install_summary(&[
                report(ManifestOutcome::Written),
                report(ManifestOutcome::Kept("it starts Vicinae".to_string())),
            ]),
            Ok("1 browser manifest(s) point to Raffi".to_string())
        );
        assert_eq!(
            install_summary(&[report(ManifestOutcome::Kept(
                "it starts Vicinae".to_string()
            ))]),
            Err("left the existing manifests alone: it starts Vicinae".to_string())
        );
        assert_eq!(
            install_summary(&[
                report(ManifestOutcome::Written),
                report(ManifestOutcome::Failed("denied".to_string())),
            ]),
            Err("/m.json: denied".to_string())
        );
        assert_eq!(
            install_summary(&[]),
            Err("no supported browser found".to_string())
        );
    }

    #[test]
    fn test_install_writes_only_for_installed_browsers_and_keeps_other_hosts() {
        let home = temp_dir("browser-host-install");
        let config = home.join(".config");
        let manifest = |dir: &str| {
            let hosts = config.join(dir).join("NativeMessagingHosts");
            fs::create_dir_all(&hosts).unwrap();
            hosts.join("com.vicinae.vicinae.json")
        };
        fs::create_dir_all(config.join("google-chrome")).unwrap();
        fs::create_dir_all(home.join(".mozilla")).unwrap();
        let chrome = config.join("google-chrome/NativeMessagingHosts/com.vicinae.vicinae.json");
        let firefox = home.join(".mozilla/native-messaging-hosts/com.vicinae.vicinae.json");
        let brave = manifest("BraveSoftware/Brave-Browser");
        let vicinae = r#"{"path": "/usr/libexec/vicinae/vicinae-browser-link"}"#;
        fs::write(&brave, vicinae).unwrap();
        let chromium = manifest("chromium");
        let foreign = r#"{"path": "/opt/another-host/native-link"}"#;
        fs::write(&chromium, foreign).unwrap();
        let vivaldi = manifest("vivaldi");
        let shared = home.join("shared.json");
        fs::write(&shared, foreign).unwrap();
        std::os::unix::fs::symlink(&shared, &vivaldi).unwrap();
        // Renamed: ownership comes from the manifest, not the file name.
        let exe = Path::new("/usr/local/bin/launcher");

        let reports = install_manifests(&home, &config, exe, false);
        let outcome = |reports: &[ManifestReport], path: &Path| {
            reports
                .iter()
                .find(|report| report.path == path)
                .map(|report| report.outcome.clone())
        };
        assert_eq!(reports.len(), 5);
        assert_eq!(outcome(&reports, &chrome), Some(ManifestOutcome::Written));
        assert_eq!(outcome(&reports, &firefox), Some(ManifestOutcome::Written));
        assert_eq!(
            outcome(&reports, &brave),
            Some(ManifestOutcome::Kept("it starts Vicinae".to_string()))
        );
        assert_eq!(
            outcome(&reports, &chromium),
            Some(ManifestOutcome::Kept(
                "it starts /opt/another-host/native-link".to_string()
            ))
        );
        assert_eq!(
            outcome(&reports, &vivaldi),
            Some(ManifestOutcome::Kept("it is a symbolic link".to_string()))
        );
        assert_eq!(fs::read_to_string(&chromium).unwrap(), foreign);
        assert_eq!(fs::read_to_string(&shared).unwrap(), foreign);

        let chrome_manifest: Value =
            serde_json::from_str(&fs::read_to_string(&chrome).unwrap()).unwrap();
        assert_eq!(chrome_manifest["path"], "/usr/local/bin/launcher");
        assert_eq!(
            chrome_manifest["allowed_origins"][0],
            "chrome-extension://kcmipingpfbohfjckomimmahknoddnke/"
        );
        let firefox_manifest: Value =
            serde_json::from_str(&fs::read_to_string(&firefox).unwrap()).unwrap();
        assert_eq!(
            firefox_manifest["allowed_extensions"][0],
            "firefox@vicinae.com"
        );

        let again = install_manifests(&home, &config, exe, true);
        assert_eq!(outcome(&again, &chrome), Some(ManifestOutcome::Unchanged));
        assert_eq!(outcome(&again, &brave), Some(ManifestOutcome::Written));
        assert_eq!(outcome(&again, &vivaldi), Some(ManifestOutcome::Written));
        assert!(
            !fs::symlink_metadata(&vivaldi)
                .unwrap()
                .file_type()
                .is_symlink(),
            "--force replaces the link instead of writing through it"
        );
        assert_eq!(fs::read_to_string(&shared).unwrap(), foreign);

        fs::write(&chrome, r#"{"path": "/opt/foreign/raffi-helper"}"#).unwrap();
        let removed = uninstall_manifests(&home, &config);
        assert_eq!(removed.len(), 4);
        assert!(chrome.exists(), "uninstall leaves other hosts alone");
        assert!(!brave.exists() && !firefox.exists() && !chromium.exists());
        assert!(fs::read_dir(chrome.parent().unwrap())
            .unwrap()
            .all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp")));

        let _ = fs::remove_dir_all(home);
    }
}
