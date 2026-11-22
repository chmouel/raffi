//! The launcher window shown by `raffi --server`.
//!
//! A server started with a display keeps iced running and opens the window
//! when `raffi` asks over the server socket, so an open skips process start,
//! the font scan and library loading. Every open loads its own session and
//! builds a fresh `LauncherApp`, as a new process would.
//!
//! The client asks only when the server can show the window exactly as the
//! client would: launched from a keybinding (no terminal), from the same
//! directory, with the same build, renderer and font. Anything else runs in
//! the client process as before.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::os::unix::net::UnixStream;
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
use crate::ServedOpen;

/// Revision of the `Open` exchange, reported in the server status.
pub(super) const WINDOW_PROTOCOL: u32 = 1;
/// How long a client waits for the server to accept an open.
const ACCEPT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the text typed after a snippet pick may take before the next
/// window can open.
const TYPING_TIMEOUT: Duration = Duration::from_secs(10);

/// What the server was started with; an open needing something else is
/// refused.
struct Host {
    renderer: crate::Renderer,
    font_family: Option<String>,
    translucent: bool,
    build: String,
    cwd: String,
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
pub(super) fn serve_open(mut stream: UnixStream, build: &str, argv: &[String], cwd: &str) {
    let job = match accept(build, argv, cwd) {
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

fn accept(build: &str, argv: &[String], cwd: &str) -> Result<Accepted, String> {
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
    let served = ServedOpen::load(argv)?;
    let settings = &served.session.settings;
    if settings.renderer != host.renderer {
        return Err("asks for another renderer than the server's".into());
    }
    if settings.font_family != host.font_family {
        return Err("asks for another font than the server's".into());
    }
    let config = &served.session.config;
    let launch = Launch::prepare(&config.entries, &config.addons, settings, host.translucent);
    let (sender, receiver) = mpsc::channel();
    Ok((
        Job {
            served,
            launch: Arc::new(launch),
            reply: sender,
        },
        receiver,
    ))
}

/// An open accepted by the server, waiting for the window loop.
struct Job {
    served: ServedOpen,
    launch: Arc<Launch>,
    reply: mpsc::Sender<Response>,
}

/// The window being shown.
struct Opened {
    /// Tells the messages of this window from those of earlier ones.
    id: u64,
    window: window::Id,
    app: LauncherApp,
    job: Job,
}

enum Stage {
    Idle,
    Open(Box<Opened>),
    /// The window closed; what was picked is running.
    Finishing(u64),
}

struct Daemon {
    stage: Stage,
    /// Opens that came while the last window was finishing.
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
            HostMessage::Job(job) => self.receive(*job),
            HostMessage::App(id, message) => match &mut self.stage {
                Stage::Open(opened) if opened.id == id => {
                    opened.app.update(message).map(app_message(id))
                }
                _ => Task::none(),
            },
            HostMessage::CloseRequested(window) => self.close(window),
            HostMessage::Closed(window) => self.closed(window),
            HostMessage::Finished(id) => {
                if !matches!(self.stage, Stage::Finishing(finishing) if finishing == id) {
                    return Task::none();
                }
                self.stage = Stage::Idle;
                let queued: Vec<Job> = self.queue.drain(..).collect();
                Task::batch(queued.into_iter().map(|job| self.receive(job)))
            }
        }
    }

    /// Open the window, close the one showing (toggle), or wait for the last
    /// one to finish.
    fn receive(&mut self, job: Job) -> Task<HostMessage> {
        match &mut self.stage {
            Stage::Idle => self.open(job),
            Stage::Open(opened) => {
                let _ = job.reply.send(Response::Toggled);
                let id = opened.id;
                opened.app.update(Message::CloseWindow).map(app_message(id))
            }
            Stage::Finishing(_) => {
                self.queue.push_back(job);
                Task::none()
            }
        }
    }

    fn open(&mut self, job: Job) -> Task<HostMessage> {
        let id = self.next_id;
        self.next_id += 1;
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
        }));
        opening.discard().chain(boot.map(app_message(id)))
    }

    /// The compositor asked to close the window: close it as Escape does.
    fn close(&mut self, window: window::Id) -> Task<HostMessage> {
        match &mut self.stage {
            Stage::Open(opened) if opened.window == window => {
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

/// Show windows for clients until the window loop fails.
pub(super) fn run(_hosting: Hosting) {
    let font_family = HOST.get().and_then(|host| host.font_family.clone());
    let mut daemon = iced::daemon(Daemon::boot, Daemon::update, Daemon::view)
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
    let build = build_identity().ok()?;
    let status = super::cache::try_request(&Request::Status, super::cache::PROBE_TIMEOUT)?;
    if !shows_windows(&status) {
        return None;
    }
    let path = super::cache::cache_socket_path().ok()?;
    let mut stream = UnixStream::connect(path).ok()?;
    let _ = stream.set_write_timeout(Some(ACCEPT_TIMEOUT));
    let mut line = serde_json::to_vec(&Request::Open { build, argv, cwd }).ok()?;
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
