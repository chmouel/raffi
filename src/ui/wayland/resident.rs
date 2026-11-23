//! The launcher window shown by `raffi --server`.
//!
//! A server started with a display keeps iced running and opens the window
//! when `raffi` asks over the server socket, so an open skips process start,
//! the font scan and library loading. Every open loads its own session and
//! builds a fresh `LauncherApp`, as a new process would.
//!
//! The client asks only when the server can show the window exactly as the
//! client would: launched from a keybinding (no terminal), from the same
//! directory and session, with the same build, renderer and font. Anything
//! else runs in the client process as before. The client's environment is
//! sent along: Raffi reads it, and the programs it starts get it, while that
//! client's window runs.

use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::os::unix::net::UnixStream;
use std::process::Stdio;
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::Result;
use iced::futures::channel::mpsc as channel;
use iced::futures::stream::{self, BoxStream, StreamExt};
use iced::{window, Element, Subscription, Task};

use super::cache::{Request, Response};
use super::state::LauncherApp;
use super::types::Message;
use super::Launch;
use crate::process::ClientEnv;
use crate::ServedOpen;

/// Revision of the `Open` exchange, reported in the server status.
pub(super) const WINDOW_PROTOCOL: u32 = 2;
/// How long a client waits for the server to accept an open.
const ACCEPT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the text typed after a snippet pick may take before the next
/// window can open.
const TYPING_TIMEOUT: Duration = Duration::from_secs(10);
/// Variables that place a client in a session: when one differs from the
/// server's, the window would open on another display or use other files,
/// so the client shows it itself.
const SESSION_VARIABLES: [&str; 11] = [
    "HOME",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_CACHE_HOME",
    "XDG_STATE_HOME",
    "XDG_RUNTIME_DIR",
    "WAYLAND_DISPLAY",
    "DISPLAY",
    "HYPRLAND_INSTANCE_SIGNATURE",
    "NIRI_SOCKET",
    "SWAYSOCK",
];

/// What the server was started with; an open needing something else is
/// refused.
struct Host {
    renderer: crate::Renderer,
    font_family: Option<String>,
    translucent: bool,
    build: String,
    cwd: String,
    /// The server's own environment, before it set the renderer.
    env: HashMap<String, String>,
}

static HOST: OnceLock<Host> = OnceLock::new();
/// Hands accepted opens to the window loop, while it runs.
static JOBS: Mutex<Option<channel::UnboundedSender<Job>>> = Mutex::new(None);
/// The other end of `JOBS`, taken by the window loop when it starts.
static RECEIVER: Mutex<Option<channel::UnboundedReceiver<Job>>> = Mutex::new(None);

/// Proof that `prepare` set the server up to show the window.
pub(super) struct Hosting(());

/// Get the server ready to show the window when it has a display. Must run
/// before any thread starts: it sets the renderer in the environment.
pub(super) fn prepare(settings: crate::ui::UISettings) -> Option<Hosting> {
    let limits = service_limits();
    if !limits.is_empty() {
        eprintln!(
            "raffi: the service would restrict the programs the launcher starts ({}), \
             so the server does not show the launcher window. Install the unit from \
             misc/systemd/raffi-server.service, then run systemctl --user daemon-reload \
             and restart the server",
            limits.join(", ")
        );
        return None;
    }
    let env = std::env::vars_os()
        .filter_map(|(name, value)| Some((name.into_string().ok()?, value.into_string().ok()?)))
        .collect();
    let display = ["WAYLAND_DISPLAY", "DISPLAY"]
        .iter()
        .any(|name| std::env::var_os(name).is_some_and(|value| !value.is_empty()));
    if !display {
        eprintln!("raffi: no display, the server does not show the launcher window");
        return None;
    }
    let build = match build_identity() {
        Ok(build) => build,
        Err(error) => {
            eprintln!("raffi: cannot identify this build, not showing the window: {error}");
            return None;
        }
    };
    let cwd = std::env::current_dir()
        .ok()
        .and_then(|cwd| cwd.into_os_string().into_string().ok())?;
    let translucent = super::select_renderer(settings.renderer);
    super::prewarm_fonts();
    let _ = HOST.set(Host {
        renderer: settings.renderer,
        font_family: settings.font_family,
        translucent,
        build,
        cwd,
        env,
    });
    let (sender, receiver) = channel::unbounded();
    *lock(&JOBS) = Some(sender);
    *lock(&RECEIVER) = Some(receiver);
    crate::process::set_hosting_window();
    Some(Hosting(()))
}

/// The `Open` revision this server speaks, 0 when it shows no window.
pub(super) fn window_protocol() -> u32 {
    if lock(&JOBS).is_some() {
        WINDOW_PROTOCOL
    } else {
        0
    }
}

/// Service settings that would stop programs the launcher starts from
/// working as they do from a keybinding: a read-only or private home and
/// `/tmp`, no privilege escalation, or being killed with the server.
fn service_limits() -> Vec<String> {
    let mut limits = Vec::new();
    let no_new_privileges = std::fs::read_to_string("/proc/self/status").is_ok_and(|status| {
        status
            .lines()
            .any(|line| line.split_whitespace().eq(["NoNewPrivs:", "1"]))
    });
    if no_new_privileges {
        limits.push("NoNewPrivileges=yes".to_string());
    }
    let Some(unit) = service_unit() else {
        return limits;
    };
    let shown = crate::process::command("systemctl")
        .args(["--user", "show", &unit])
        .args(["-p", "KillMode", "-p", "PrivateTmp"])
        .args(["-p", "ProtectHome", "-p", "ProtectSystem"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    if let Some(output) = shown.ok().filter(|output| output.status.success()) {
        limits.extend(limiting_properties(&String::from_utf8_lossy(
            &output.stdout,
        )));
    }
    limits
}

/// The systemd service this process runs in, if any.
fn service_unit() -> Option<String> {
    let cgroup = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let path = cgroup.lines().find_map(|line| line.strip_prefix("0::"))?;
    let unit = path.rsplit('/').next()?;
    (unit.ends_with(".service") && !unit.starts_with("user@")).then(|| unit.to_string())
}

/// The lines of `systemctl show` output that restrict launched programs.
fn limiting_properties(properties: &str) -> Vec<String> {
    properties
        .lines()
        .filter(|line| match line.split_once('=') {
            Some(("KillMode", mode)) => mode != "process",
            Some(("PrivateTmp" | "ProtectHome" | "ProtectSystem", value)) => value != "no",
            _ => false,
        })
        .map(str::to_string)
        .collect()
}

/// The first session variable that differs between the server and a client.
fn session_difference(
    server: &HashMap<String, String>,
    client: &HashMap<String, String>,
) -> Option<&'static str> {
    SESSION_VARIABLES
        .into_iter()
        .find(|name| server.get(*name) != client.get(*name))
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

/// The executable this process runs, by device, inode and modification
/// time: a rebuilt binary differs even at the same path.
fn build_identity() -> std::io::Result<String> {
    let metadata = std::fs::metadata("/proc/self/exe")?;
    Ok(format!(
        "{}:{}:{}.{}",
        metadata.dev(),
        metadata.ino(),
        metadata.mtime(),
        metadata.mtime_nsec()
    ))
}

fn write_response(stream: &mut UnixStream, response: &Response) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(response).map_err(std::io::Error::other)?;
    line.push(b'\n');
    stream.write_all(&line)
}

/// Answer an `Open` request on a server socket thread: refuse it, or hand
/// it to the window loop and report how it ended.
pub(super) fn serve_open(
    mut stream: UnixStream,
    build: &str,
    argv: &[String],
    cwd: &str,
    env: HashMap<String, String>,
) {
    let job = match accept(build, argv, cwd, env) {
        Ok(job) => job,
        Err(reason) => {
            eprintln!("raffi: showing the window in the client: {reason}");
            let _ = write_response(&mut stream, &Response::Refused { reason });
            return;
        }
    };
    let (job, reply) = job;
    let sent = lock(&JOBS)
        .as_ref()
        .is_some_and(|jobs| jobs.unbounded_send(job).is_ok());
    if !sent {
        let reason = "the server stopped showing the window".to_string();
        let _ = write_response(&mut stream, &Response::Refused { reason });
        return;
    }
    if write_response(&mut stream, &Response::Accepted).is_err() {
        return;
    }
    let response = reply.recv().unwrap_or_else(|_| Response::Done {
        error: Some("the server stopped showing the window".into()),
    });
    let _ = write_response(&mut stream, &response);
}

type Accepted = (Job, mpsc::Receiver<Response>);

fn accept(
    build: &str,
    argv: &[String],
    cwd: &str,
    env: HashMap<String, String>,
) -> Result<Accepted, String> {
    let host = HOST.get().ok_or("the server shows no window")?;
    if lock(&JOBS).is_none() {
        return Err("the server shows no window".into());
    }
    if build != host.build {
        return Err("raffi was rebuilt since the server started: restart the server".into());
    }
    if cwd != host.cwd {
        return Err("started from another directory than the server".into());
    }
    if let Some(name) = session_difference(&host.env, &env) {
        return Err(format!("{name} differs from the server's"));
    }
    let env = ClientEnv::new(env);
    let (served, launch) = crate::process::with_env(env.clone(), || {
        let served = ServedOpen::load(argv)?;
        let settings = &served.session.settings;
        if settings.renderer != host.renderer {
            return Err("asks for another renderer than the server's".to_string());
        }
        if settings.font_family != host.font_family {
            return Err("asks for another font than the server's".into());
        }
        let config = &served.session.config;
        let launch = Launch::prepare(&config.entries, &config.addons, settings, host.translucent);
        Ok((served, launch))
    })?;
    let (sender, receiver) = mpsc::channel();
    Ok((
        Job {
            served,
            launch: Arc::new(launch),
            env,
            reply: sender,
        },
        receiver,
    ))
}

/// An open accepted by the server, waiting for the window loop.
struct Job {
    served: ServedOpen,
    launch: Arc<Launch>,
    env: ClientEnv,
    reply: mpsc::Sender<Response>,
}

/// The window being shown.
struct Opened {
    /// Tells the messages of this window from those of earlier ones.
    id: u64,
    window: window::Id,
    app: LauncherApp,
    job: Job,
    /// The window exists: an open arriving earlier waits instead of closing
    /// a window not there yet.
    shown: bool,
    /// An open already asked the window to close.
    closing: bool,
}

enum Stage {
    Idle,
    Open(Box<Opened>),
    /// The window closed; what was picked is running.
    Finishing(u64),
}

struct Daemon {
    stage: Stage,
    /// Opens waiting for the window to show or for the last one to finish,
    /// taken one at a time.
    queue: VecDeque<Job>,
    next_id: u64,
}

// Most messages are the window's own; boxing them would cost an allocation
// each.
#[allow(clippy::large_enum_variant)]
enum HostMessage {
    Job(Box<Job>),
    App(u64, Message),
    CloseRequested(window::Id),
    Closed(window::Id),
    Shown(u64),
    Finished(u64),
}

impl Daemon {
    fn boot() -> (Self, Task<HostMessage>) {
        (
            Self {
                stage: Stage::Idle,
                queue: VecDeque::new(),
                next_id: 0,
            },
            Task::none(),
        )
    }

    fn update(&mut self, message: HostMessage) -> Task<HostMessage> {
        match message {
            // Opens are taken in order: one arriving behind others waits.
            HostMessage::Job(job) if !self.queue.is_empty() => {
                self.queue.push_back(*job);
                Task::none()
            }
            HostMessage::Job(job) => self.receive(*job),
            HostMessage::App(id, message) => match &mut self.stage {
                Stage::Open(opened) if opened.id == id => {
                    opened.app.update(message).map(app_message(id))
                }
                _ => Task::none(),
            },
            HostMessage::CloseRequested(window) => self.close(window),
            HostMessage::Closed(window) => self.closed(window),
            HostMessage::Shown(id) => match &mut self.stage {
                Stage::Open(opened) if opened.id == id => {
                    opened.shown = true;
                    self.next_queued()
                }
                _ => Task::none(),
            },
            HostMessage::Finished(id) => {
                if !matches!(self.stage, Stage::Finishing(finishing) if finishing == id) {
                    return Task::none();
                }
                self.stage = Stage::Idle;
                crate::process::set_served_env(None);
                self.next_queued()
            }
        }
    }

    fn next_queued(&mut self) -> Task<HostMessage> {
        match self.queue.pop_front() {
            Some(job) => self.receive(job),
            None => Task::none(),
        }
    }

    /// Open the window, close the one showing (toggle), or wait until the
    /// window shows or the last one finishes.
    fn receive(&mut self, job: Job) -> Task<HostMessage> {
        match &mut self.stage {
            Stage::Idle => self.open(job),
            Stage::Open(opened) if opened.shown && !opened.closing => {
                let _ = job.reply.send(Response::Toggled);
                opened.closing = true;
                let id = opened.id;
                opened.app.update(Message::CloseWindow).map(app_message(id))
            }
            _ => {
                self.queue.push_back(job);
                Task::none()
            }
        }
    }

    fn open(&mut self, job: Job) -> Task<HostMessage> {
        let id = self.next_id;
        self.next_id += 1;
        crate::process::set_served_env(Some(job.env.clone()));
        let slack = &job.served.session.config.addons.slack;
        super::slack_api::select_window_client(
            slack.token_command.as_deref(),
            slack.cookie_command.as_deref(),
        );
        let (app, boot) = job.launch.boot(true);
        let mut settings = job.launch.window_settings();
        settings.exit_on_close_request = false;
        let (window, opening) = window::open(settings);
        crate::debug_log!("server: opening window {id}");
        self.stage = Stage::Open(Box::new(Opened {
            id,
            window,
            app,
            job,
            shown: false,
            closing: false,
        }));
        opening
            .map(move |_| HostMessage::Shown(id))
            .chain(boot.map(app_message(id)))
    }

    /// The compositor asked to close the window: close it as Escape does.
    fn close(&mut self, window: window::Id) -> Task<HostMessage> {
        match &mut self.stage {
            Stage::Open(opened) if opened.window == window => {
                opened.closing = true;
                let id = opened.id;
                opened.app.update(Message::CloseWindow).map(app_message(id))
            }
            _ => Task::none(),
        }
    }

    /// The window is gone: run what was picked off the event loop, then
    /// report to the client that opened it.
    fn closed(&mut self, window: window::Id) -> Task<HostMessage> {
        if !matches!(&self.stage, Stage::Open(opened) if opened.window == window) {
            return Task::none();
        }
        let Stage::Open(opened) = std::mem::replace(&mut self.stage, Stage::Idle) else {
            return Task::none();
        };
        let Opened { id, app, job, .. } = *opened;
        drop(app);
        self.stage = Stage::Finishing(id);
        Task::perform(
            super::browser_data::off_thread(move || finish(job)),
            move |_| HostMessage::Finished(id),
        )
    }

    fn view(&self, window: window::Id) -> Element<'_, HostMessage> {
        match &self.stage {
            Stage::Open(opened) if opened.window == window => {
                opened.app.view().map(app_message(opened.id))
            }
            _ => iced::widget::Space::new().into(),
        }
    }

    fn subscription(&self) -> Subscription<HostMessage> {
        let mut subscriptions = vec![
            Subscription::run(jobs),
            window::close_events().map(HostMessage::Closed),
            window::close_requests().map(HostMessage::CloseRequested),
        ];
        if let Stage::Open(opened) = &self.stage {
            subscriptions.push(
                opened
                    .app
                    .subscription()
                    .with(opened.id)
                    .map(|(id, message)| HostMessage::App(id, message)),
            );
        }
        Subscription::batch(subscriptions)
    }

    fn launch(&self) -> Option<&Launch> {
        match &self.stage {
            Stage::Open(opened) => Some(&opened.job.launch),
            _ => None,
        }
    }

    fn theme(&self, _window: window::Id) -> iced::Theme {
        self.launch()
            .map_or(iced::Theme::Dark, |launch| launch.iced_theme())
    }

    fn style(&self, theme: &iced::Theme) -> iced::theme::Style {
        match self.launch() {
            Some(launch) => launch.style(theme),
            None => iced::theme::Style {
                background_color: iced::Color::TRANSPARENT,
                text_color: theme.palette().text,
            },
        }
    }
}

fn app_message(id: u64) -> impl Fn(Message) -> HostMessage + Clone + Send + 'static {
    move |message| HostMessage::App(id, message)
}

fn jobs() -> BoxStream<'static, HostMessage> {
    match lock(&RECEIVER).take() {
        Some(receiver) => receiver.map(|job| HostMessage::Job(Box::new(job))).boxed(),
        None => stream::pending().boxed(),
    }
}

/// Run what the window left to do, as the client would after its window,
/// and wait for the text a snippet types so it cannot land in the next
/// window.
fn finish(job: Job) {
    let picked = job
        .launch
        .finish()
        .and_then(|chosen| job.served.launch(&chosen))
        .map_err(|error| format!("{error:#}"));
    let typed = super::actions::wait_for_typing(TYPING_TIMEOUT);
    let error = picked.err().or(typed.err());
    if let Some(error) = &error {
        eprintln!("raffi: {error}");
    }
    let _ = job.reply.send(Response::Done { error });
}

/// iced's executor, running each task and subscription in the environment
/// current when it was started, so work begun for a window keeps that
/// caller's environment after the window closes and another one opens.
struct ServedExecutor(iced::executor::Default);

impl iced::Executor for ServedExecutor {
    fn new() -> Result<Self, iced::futures::io::Error> {
        iced::executor::Default::new().map(Self)
    }

    fn spawn(
        &self,
        future: impl std::future::Future<Output = ()> + iced_futures::MaybeSend + 'static,
    ) {
        self.0.spawn(InContext {
            context: crate::process::context(),
            future: Box::pin(future),
        });
    }

    fn block_on<T>(&self, future: impl std::future::Future<Output = T>) -> T {
        self.0.block_on(future)
    }

    fn enter<R>(&self, f: impl FnOnce() -> R) -> R {
        self.0.enter(f)
    }
}

struct InContext<F> {
    context: crate::process::Context,
    future: std::pin::Pin<Box<F>>,
}

impl<F: std::future::Future> std::future::Future for InContext<F> {
    type Output = F::Output;

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let this = &mut *self;
        this.context.enter(|| this.future.as_mut().poll(cx))
    }
}

/// Show windows for clients until the window loop fails.
pub(super) fn run(_hosting: Hosting) {
    let font_family = HOST.get().and_then(|host| host.font_family.clone());
    let mut daemon = iced::daemon(Daemon::boot, Daemon::update, Daemon::view)
        .executor::<ServedExecutor>()
        .subscription(Daemon::subscription)
        .theme(Daemon::theme)
        .style(Daemon::style);
    if let Some(family) = font_family {
        daemon = daemon.default_font(super::default_font(&family));
    }
    eprintln!("raffi: the server shows the launcher window");
    let result = daemon.run();
    lock(&JOBS).take();
    if let Err(error) = result {
        eprintln!("raffi: the launcher window stopped: {error}");
    }
}

/// Whether fds 0-2 are all `/dev/null`, as when a keybinding starts Raffi.
/// Runs from a terminal or with redirected streams stay in the process.
fn streams_are_null() -> bool {
    let Ok(null) = std::fs::metadata("/dev/null") else {
        return false;
    };
    ["/proc/self/fd/0", "/proc/self/fd/1", "/proc/self/fd/2"]
        .iter()
        .all(|fd| {
            std::fs::metadata(fd).is_ok_and(|metadata| {
                metadata.file_type().is_char_device() && metadata.rdev() == null.rdev()
            })
        })
}

/// Ask the server to show the window. `None` when it certainly did nothing,
/// and the caller shows the window itself; once the request is sent, the
/// outcome is the server's, so a pick never runs twice.
pub(crate) fn open_in_server() -> Option<Result<()>> {
    if !streams_are_null() {
        return None;
    }
    let argv = std::env::args_os()
        .map(|arg| arg.into_string().ok())
        .collect::<Option<Vec<String>>>()?;
    let cwd = std::env::current_dir()
        .ok()?
        .into_os_string()
        .into_string()
        .ok()?;
    let env = std::env::vars_os()
        .map(|(name, value)| Some((name.into_string().ok()?, value.into_string().ok()?)))
        .collect::<Option<HashMap<String, String>>>()?;
    let build = build_identity().ok()?;
    let status = super::cache::try_request(&Request::Status, super::cache::PROBE_TIMEOUT)?;
    if !shows_windows(&status) {
        return None;
    }
    let path = super::cache::cache_socket_path().ok()?;
    let mut stream = UnixStream::connect(path).ok()?;
    let _ = stream.set_write_timeout(Some(ACCEPT_TIMEOUT));
    let mut line = serde_json::to_vec(&Request::Open {
        build,
        argv,
        cwd,
        env,
    })
    .ok()?;
    line.push(b'\n');
    stream.write_all(&line).ok()?;
    await_window(stream)
}

/// Whether a server answering `status` takes `Open` requests. Servers from
/// before the window exchange do not report it and are never sent one.
fn shows_windows(status: &Response) -> bool {
    matches!(status, Response::Status { window, .. } if *window == WINDOW_PROTOCOL)
}

/// Follow an `Open` the server received to its end.
fn await_window(stream: UnixStream) -> Option<Result<()>> {
    let _ = stream.set_read_timeout(Some(ACCEPT_TIMEOUT));
    let mut reader = BufReader::new(stream);
    match read_response(&mut reader) {
        Ok(Response::Refused { reason }) => {
            crate::debug_log!("server: showing the window here: {reason}");
            return None;
        }
        Ok(Response::Accepted) => {}
        Ok(_) => {
            return Some(Err(anyhow::anyhow!(
                "unexpected answer from the Raffi server"
            )))
        }
        Err(error) => return Some(Err(error)),
    }
    let _ = reader.get_ref().set_read_timeout(None);
    Some(match read_response(&mut reader) {
        Ok(Response::Done { error: None } | Response::Toggled) => Ok(()),
        Ok(Response::Done { error: Some(error) }) => Err(anyhow::anyhow!(error)),
        Ok(_) => Err(anyhow::anyhow!("unexpected answer from the Raffi server")),
        Err(error) => Err(error),
    })
}

fn read_response(reader: &mut impl BufRead) -> Result<Response> {
    let mut line = String::new();
    reader.read_line(&mut line)?;
    if line.is_empty() {
        anyhow::bail!("the Raffi server closed the connection");
    }
    Ok(serde_json::from_str(&line)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exchange(lines: &[&str]) -> Option<Result<()>> {
        let (client, mut server) = UnixStream::pair().unwrap();
        for line in lines {
            server.write_all(line.as_bytes()).unwrap();
            server.write_all(b"\n").unwrap();
        }
        drop(server);
        await_window(client)
    }

    #[test]
    fn test_clients_from_another_session_show_the_window_themselves() {
        let env = |pairs: &[(&str, &str)]| -> HashMap<String, String> {
            pairs
                .iter()
                .map(|(name, value)| (name.to_string(), value.to_string()))
                .collect()
        };
        let server = env(&[("HOME", "/home/u"), ("WAYLAND_DISPLAY", "wayland-1")]);
        // Variables Raffi forwards, such as PATH or the session id, may differ.
        let keybinding = env(&[
            ("HOME", "/home/u"),
            ("WAYLAND_DISPLAY", "wayland-1"),
            ("XDG_SESSION_ID", "2"),
            ("PATH", "/client/bin"),
        ]);
        assert_eq!(session_difference(&server, &keybinding), None);
        let other_display = env(&[("HOME", "/home/u"), ("WAYLAND_DISPLAY", "wayland-2")]);
        assert_eq!(
            session_difference(&server, &other_display),
            Some("WAYLAND_DISPLAY")
        );
        let x11 = env(&[
            ("HOME", "/home/u"),
            ("WAYLAND_DISPLAY", "wayland-1"),
            ("DISPLAY", ":1"),
        ]);
        assert_eq!(session_difference(&server, &x11), Some("DISPLAY"));
    }

    #[test]
    fn test_service_settings_restricting_launches_are_reported() {
        let current = "KillMode=process\nPrivateTmp=no\nProtectHome=no\nProtectSystem=no\n";
        assert!(limiting_properties(current).is_empty());
        let sandboxed =
            "KillMode=control-group\nPrivateTmp=yes\nProtectHome=read-only\nProtectSystem=strict\n";
        assert_eq!(
            limiting_properties(sandboxed),
            [
                "KillMode=control-group",
                "PrivateTmp=yes",
                "ProtectHome=read-only",
                "ProtectSystem=strict"
            ]
        );
    }

    #[test]
    fn test_open_is_sent_only_to_servers_showing_windows() {
        let status = |window: &str| -> Response {
            serde_json::from_str(&format!(
                r#"{{"type":"status","pid":1,"version":"x","config_digest":"x","sources":[],"protocol":6{window}}}"#
            ))
            .unwrap()
        };
        assert!(!shows_windows(&status("")));
        assert!(!shows_windows(&status(r#","window":0"#)));
        assert!(shows_windows(&status(&format!(
            r#","window":{WINDOW_PROTOCOL}"#
        ))));
    }

    #[test]
    fn test_client_shows_the_window_itself_only_when_refused() {
        assert!(exchange(&[r#"{"type":"refused","reason":"no"}"#]).is_none());
        assert!(matches!(
            exchange(&[r#"{"type":"accepted"}"#, r#"{"type":"done"}"#]),
            Some(Ok(()))
        ));
        assert!(matches!(
            exchange(&[r#"{"type":"accepted"}"#, r#"{"type":"toggled"}"#]),
            Some(Ok(()))
        ));
        let failed = exchange(&[
            r#"{"type":"accepted"}"#,
            r#"{"type":"done","error":"boom"}"#,
        ]);
        assert_eq!(failed.unwrap().unwrap_err().to_string(), "boom");
        // Once the server accepted, a lost answer is an error, never a
        // second window that could run the pick again.
        assert!(matches!(
            exchange(&[r#"{"type":"accepted"}"#]),
            Some(Err(_))
        ));
        assert!(matches!(exchange(&[]), Some(Err(_))));
    }
}
