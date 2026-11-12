//! `raffi clipboard`: the clipboard history from the command line.
//!
//! Every command works on the history `addons.clipboard.backend` selects,
//! resolved once per invocation so ids from one backend never reach another.
//! Ids are opaque: 32 hex characters for the `raffi` backend, 64 for CopyQ. A
//! reference is either an index of the default listing or the start of an id.
//!
//! Text meant for a terminal goes through [`sanitize`], so a copied escape
//! sequence cannot rewrite the screen. Only `show` writes an entry's own
//! bytes, and it refuses to do so on a terminal.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, Read, Write};
use std::time::Duration;

use chrono::{NaiveDateTime, TimeZone};
use serde::Serialize;
use unicode_width::UnicodeWidthStr;

use super::clipboard::{
    local_time, one_line, parse_uri_list, short_copy_time, Backend, ClipItem, CopyQ, History,
    ItemKind, Page, LINE_BREAK_MARK,
};
use super::clipboard_store::{self, kind_name, kind_predicate, now_secs, ImageMeta, Store};
use super::preview::format_byte_size;
use crate::{
    ClipboardAddonConfig, ClipboardArgs, ClipboardClearArgs, ClipboardCommand, ClipboardKind,
    ClipboardListArgs, ClipboardSetArgs, ClipboardShowArgs, ClipboardStatusArgs,
};

/// Longest id prefix the table shows. A prefix reference needs at least
/// [`MIN_PREFIX`] characters and may be the whole id.
const SHORT_ID_CHARS: usize = 10;
const MIN_PREFIX: usize = 6;
/// An all-digit reference is an index of the default listing, up to this many
/// digits.
const MAX_INDEX_DIGITS: usize = 5;
const PREVIEW_CHARS: usize = 160;
/// Column budget assumed when the terminal does not say how wide it is.
const DEFAULT_WIDTH: usize = 100;
/// Least the preview column keeps, whatever the terminal width.
const MIN_PREVIEW_CHARS: usize = 12;
/// A deletion's log checkpoint is retried a few times before the leftovers
/// are reported.
const CHECKPOINT_RETRIES: usize = 5;
const CHECKPOINT_RETRY_DELAY: Duration = Duration::from_millis(100);
const TEXT_FORMATS: [&str; 2] = ["text/plain", "text/plain;charset=utf-8"];
/// MIME type `set` puts text on the clipboard as when none is asked for.
const DEFAULT_MIME: &str = "text/plain";

/// `raffi clipboard`, dispatched from `run()` once the config is loaded.
pub(crate) fn run_clipboard_cli(
    config: &ClipboardAddonConfig,
    args: &ClipboardArgs,
) -> Result<(), Error> {
    // Writing is part of the command, so a broken pipe is reported the way
    // the command failed, JSON and all.
    let result = execute(config, args, Io::detect()).and_then(write_output);
    match result {
        Ok(()) => Ok(()),
        Err(error) if json_requested(args) => {
            eprintln!("{}", error.json_line());
            Err(error.reported())
        }
        Err(error) => Err(error),
    }
}

/// Write what a command produced, then its notices on stderr.
fn write_output(output: Output) -> Result<(), Error> {
    let mut stdout = std::io::stdout().lock();
    write_stdout(&mut stdout, &output.stdout)?;
    drop(stdout);
    for notice in &output.notices {
        eprintln!("{notice}");
    }
    Ok(())
}

/// Write and flush `bytes`, so a full disk or a closed pipe is a failure of
/// the command rather than something the process reports late.
fn write_stdout(stream: &mut impl Write, bytes: &[u8]) -> Result<(), Error> {
    stream
        .write_all(bytes)
        .and_then(|()| stream.flush())
        .map_err(|error| Error::new(Code::Io, format!("cannot write to stdout: {error}")))
}

/// Whether the action prints JSON instead of text, and so gets its errors as
/// JSON too.
fn json_requested(args: &ClipboardArgs) -> bool {
    match &args.action {
        Some(ClipboardCommand::List(list)) => list.json,
        Some(ClipboardCommand::Show(show)) => show.json,
        Some(ClipboardCommand::Set(set)) => set.json,
        Some(ClipboardCommand::Status(status)) => status.json,
        _ => false,
    }
}

/// Why a command failed, as the stable value `--json` prints under `code`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Code {
    /// No entry answers the reference.
    NotFound,
    /// An id prefix names several entries.
    AmbiguousRef,
    /// The reference is not an index, an id prefix or `-`.
    InvalidRef,
    /// The command only takes ids, not the position of a listing.
    IndexNotAllowed,
    /// No clipboard history service is running.
    NoBackend,
    /// The backend needs `raffi --cache-daemon`, which is not answering.
    RecorderNotRunning,
    /// The running service is from an older Raffi and cannot set the
    /// clipboard.
    DaemonOutdated,
    /// The backend does not do this, or refuses in this situation.
    Unsupported,
    /// The content is larger than the limit of its type.
    TooLarge,
    /// There is no clipboard history to read.
    NoHistory,
    /// Reading, writing or talking to the backend failed.
    Io,
    /// A value given on the command line does not make sense.
    InvalidArgument,
    /// A failure that fits none of the above.
    Other,
}

impl Code {
    fn name(self) -> &'static str {
        match self {
            Self::NotFound => "not_found",
            Self::AmbiguousRef => "ambiguous_ref",
            Self::InvalidRef => "invalid_ref",
            Self::IndexNotAllowed => "index_not_allowed",
            Self::NoBackend => "no_backend",
            Self::RecorderNotRunning => "recorder_not_running",
            Self::DaemonOutdated => "daemon_outdated",
            Self::Unsupported => "unsupported",
            Self::TooLarge => "too_large",
            Self::NoHistory => "no_history",
            Self::Io => "io",
            Self::InvalidArgument => "invalid_argument",
            Self::Other => "other",
        }
    }
}

/// A failed command: the message a reader sees, the stable code a script
/// reads from `--json`, and whether the CLI has already written it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Error {
    code: Code,
    message: String,
    /// A usage error exits 2, as the options parser does, and any other 1.
    usage: bool,
    reported: bool,
}

impl Error {
    fn new(code: Code, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            usage: false,
            reported: false,
        }
    }

    /// An invalid value, which exits 2 like the options parser.
    fn usage(message: impl Into<String>) -> Self {
        Self {
            usage: true,
            ..Self::new(Code::InvalidArgument, message)
        }
    }

    fn reported(mut self) -> Self {
        self.reported = true;
        self
    }

    /// Whether the CLI already wrote the diagnostic.
    pub(crate) fn is_reported(&self) -> bool {
        self.reported
    }

    pub(crate) fn exit_code(&self) -> i32 {
        if self.usage {
            2
        } else {
            1
        }
    }

    /// The one line `--json` prints on stderr.
    fn json_line(&self) -> String {
        format!(
            "{{\"error\":{},\"code\":{}}}",
            serde_json::Value::String(self.message.clone()),
            serde_json::Value::String(self.code.name().to_string())
        )
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

/// A failure raised elsewhere: a database, a pipe or a backend call. The two
/// messages the backends are recognised by keep their own code.
impl From<String> for Error {
    fn from(message: String) -> Self {
        let code = if message == super::clipboard::NO_BACKEND {
            Code::NoBackend
        } else if message == super::cache::CLIPBOARD_NOT_RUNNING {
            Code::RecorderNotRunning
        } else if clipboard_store::is_gone(&message) {
            Code::NotFound
        } else {
            Code::Io
        };
        Self::new(code, message)
    }
}

/// A failed `clipboard set` keeps the code the service gave it.
impl From<super::cache::SetFailure> for Error {
    fn from(failure: super::cache::SetFailure) -> Self {
        use super::cache::{ErrorCode, SetFailure};
        let code = match &failure {
            SetFailure::DaemonOutdated(_) => Code::DaemonOutdated,
            SetFailure::NotRunning => Code::RecorderNotRunning,
            SetFailure::Failed(Some(ErrorCode::TooLarge), _) => Code::TooLarge,
            SetFailure::Failed(Some(ErrorCode::InvalidArgument), _) => Code::InvalidArgument,
            SetFailure::Failed(Some(ErrorCode::NotRecording), _) => Code::RecorderNotRunning,
            SetFailure::Failed(Some(ErrorCode::NotFound), _) => Code::NotFound,
            SetFailure::Failed(_, _) => Code::Io,
        };
        Self::new(code, failure.to_string())
    }
}

/// What a command wrote: the bytes for stdout, and the lines for stderr.
#[derive(Debug, Default, PartialEq, Eq)]
struct Output {
    stdout: Vec<u8>,
    notices: Vec<String>,
}

impl Output {
    fn text(text: impl Into<String>) -> Self {
        Self::bytes(text.into().into_bytes())
    }

    fn bytes(bytes: Vec<u8>) -> Self {
        Self {
            stdout: bytes,
            notices: Vec::new(),
        }
    }

    fn notice(mut self, notice: impl Into<String>) -> Self {
        self.notices.push(notice.into());
        self
    }
}

/// What a command may know about the streams it writes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Io {
    stdout_tty: bool,
    stderr_tty: bool,
    stdin_tty: bool,
    width: usize,
    color: bool,
}

impl Io {
    fn detect() -> Self {
        let stdout_tty = crate::ui::is_tty(libc::STDOUT_FILENO);
        let width = std::env::var("COLUMNS")
            .ok()
            .and_then(|columns| columns.trim().parse().ok())
            .filter(|width| *width > MIN_PREVIEW_CHARS * 4)
            .unwrap_or(DEFAULT_WIDTH);
        Self {
            stdout_tty,
            stderr_tty: crate::ui::is_tty(libc::STDERR_FILENO),
            stdin_tty: crate::ui::is_tty(libc::STDIN_FILENO),
            width,
            color: stdout_tty && std::env::var_os("NO_COLOR").is_none(),
        }
    }

    #[cfg(test)]
    fn pipe() -> Self {
        Self {
            stdout_tty: false,
            stderr_tty: false,
            stdin_tty: false,
            width: DEFAULT_WIDTH,
            color: false,
        }
    }
}

/// The instant a command reads: unix seconds for deadlines, local time for
/// the dates of the listing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Clock {
    secs: i64,
    local: NaiveDateTime,
}

impl Clock {
    fn now() -> Self {
        Self::at(now_secs())
    }

    fn at(secs: i64) -> Self {
        let local = chrono::Local
            .timestamp_opt(secs, 0)
            .single()
            .map(|time| time.naive_local())
            .unwrap_or_default();
        Self { secs, local }
    }
}

/// The clipboard settings a command honours.
struct Settings {
    min_chars: usize,
    exclude: Vec<regex::Regex>,
    limits: clipboard_store::Limits,
    backend: crate::ClipboardBackend,
}

impl Settings {
    fn from_config(config: &ClipboardAddonConfig) -> Result<Self, Error> {
        Ok(Self {
            min_chars: config.min_chars(),
            exclude: config
                .exclude_regexes()
                .map_err(|message| Error::new(Code::InvalidArgument, message))?,
            limits: clipboard_store::Limits::from_config(config),
            backend: config.backend,
        })
    }
}

/// The history a command reads, opened once for the whole invocation.
enum Clips {
    /// `None` when this installation has no database yet.
    Raffi(Option<Store>),
    CopyQ(CopyQ),
}

impl Clips {
    fn open(backend: Backend) -> Result<Self, Error> {
        Ok(match backend {
            Backend::Raffi => Self::Raffi(Store::open_existing()?),
            Backend::CopyQ(copyq) => Self::CopyQ(copyq),
        })
    }

    /// Name of the backend that was resolved.
    fn name(&self) -> &'static str {
        match self {
            Self::Raffi(_) => "raffi",
            Self::CopyQ(_) => "copyq",
        }
    }

    fn is_copyq(&self) -> bool {
        matches!(self, Self::CopyQ(_))
    }

    /// One listing of the history with `query` and `kind` applied.
    fn list(
        &mut self,
        query: &str,
        kind: Option<ClipboardKind>,
        min_chars: usize,
        exclude: &[regex::Regex],
        now: i64,
    ) -> Result<Page, Error> {
        let only_kind = kind.map_or("", ClipboardKind::name);
        match self {
            Self::Raffi(Some(store)) => store
                .list_until(query, only_kind, min_chars, exclude, now, None)
                .map_err(Error::from),
            Self::Raffi(None) => Ok(empty_page()),
            Self::CopyQ(copyq) => copyq_listing(copyq, query, only_kind, min_chars, exclude),
        }
    }

    /// The pixel size of an image entry, when the backend can give it
    /// without reading the whole picture. A CopyQ history has none.
    fn image_meta(&self, item: &ClipItem, now: i64) -> Option<ImageMeta> {
        if item.kind != ItemKind::Image {
            return None;
        }
        match self {
            Self::Raffi(Some(store)) => store.image_meta(&item.id, now).ok().flatten(),
            _ => None,
        }
    }
}

fn empty_page() -> Page {
    Page {
        tab: clipboard_store::TAB.to_string(),
        total: 0,
        head: String::new(),
        next: 0,
        stable: true,
        items: Vec::new(),
    }
}

/// Every matching CopyQ row, page after page, so a reference resolves against
/// the whole history. The history may change while it is scanned: the scan
/// starts over once, then gives up rather than mixing two states.
fn copyq_listing(
    copyq: &CopyQ,
    query: &str,
    only_kind: &str,
    min_chars: usize,
    exclude: &[regex::Regex],
) -> Result<Page, Error> {
    let mut fetch = |tab: &str, start: usize| {
        copyq
            .list_page(tab, start, query, only_kind, min_chars, exclude)
            .map_err(Error::from)
    };
    stable_listing(&mut fetch)
}

/// Read every page of one history, and retry the whole scan once when the
/// history moved under it.
fn stable_listing(
    fetch: &mut dyn FnMut(&str, usize) -> Result<Page, Error>,
) -> Result<Page, Error> {
    for _ in 0..2 {
        if let Some(page) = enumerate_pages(fetch)? {
            return Ok(page);
        }
    }
    Err(Error::new(
        Code::Io,
        "the clipboard history kept changing, try again",
    ))
}

/// Read every page of the history and join them into one listing. `None`
/// means the history changed while it was read: a page was not stable, or
/// `head` or `total` moved between pages.
fn enumerate_pages(
    fetch: &mut dyn FnMut(&str, usize) -> Result<Page, Error>,
) -> Result<Option<Page>, Error> {
    let mut items = Vec::new();
    let mut tab = String::new();
    let mut start = 0;
    let mut seen: Option<(String, usize)> = None;
    let total = loop {
        let page = fetch(&tab, start)?;
        tab = page.tab.clone();
        if !page.stable {
            return Ok(None);
        }
        match &seen {
            None => seen = Some((page.head.clone(), page.total)),
            Some((head, total)) if *head != page.head || *total != page.total => return Ok(None),
            Some(_) => {}
        }
        let (next, page_total) = (page.next, page.total);
        items.extend(page.items);
        if next >= page_total {
            break page_total;
        }
        if next <= start {
            return Err(Error::new(
                Code::Io,
                "CopyQ returned an inconsistent history listing",
            ));
        }
        start = next;
    };
    let (head, _) = seen.unwrap_or_default();
    Ok(Some(Page {
        tab,
        total,
        head,
        next: total,
        stable: true,
        items,
    }))
}

/// Everything one invocation works with.
struct Run<'a> {
    clips: Clips,
    settings: &'a Settings,
    clock: Clock,
    io: Io,
}

impl Run<'_> {
    /// The entries a reference may name: alive, and not hidden by
    /// `exclude_patterns`. `min_chars` is deliberately absent, so an entry
    /// only `list --all` shows can still be referenced.
    fn eligible(&mut self) -> Result<Page, Error> {
        self.search("", None, 0)
    }

    /// The listing `query` and `kind` select.
    fn search(
        &mut self,
        query: &str,
        kind: Option<ClipboardKind>,
        min_chars: usize,
    ) -> Result<Page, Error> {
        let exclude = self.settings.exclude.clone();
        let now = self.clock.secs;
        self.clips.list(query, kind, min_chars, &exclude, now)
    }

    /// One snapshot of the eligible set, with the positions of the default
    /// listing within it. The positions come from a second backend listing
    /// with the configured `min_chars`, so the backend's own trimmed-length
    /// predicate decides what counts, and entries recorded since the snapshot
    /// are left out of the map.
    fn snapshot(&mut self) -> Result<(Page, HashMap<String, usize>), Error> {
        let eligible = self.eligible()?;
        let min_chars = self.settings.min_chars;
        let shown = self.search("", None, min_chars)?;
        let eligible_ids: HashSet<&str> =
            eligible.items.iter().map(|item| item.id.as_str()).collect();
        let index = shown
            .items
            .iter()
            .filter(|item| eligible_ids.contains(item.id.as_str()))
            .enumerate()
            .map(|(position, item)| (item.id.clone(), position + 1))
            .collect();
        Ok((eligible, index))
    }

    /// The one entry `refs` names, its position in the default listing, and
    /// the tab it was listed from.
    fn resolve_one(&mut self, refs: &[String]) -> Result<(ClipItem, Option<usize>, String), Error> {
        let token = match refs {
            [token] => token,
            [] => return Err(Error::new(Code::InvalidRef, "no clipboard entry given")),
            _ => {
                return Err(Error::new(
                    Code::InvalidRef,
                    "this command takes one clipboard entry",
                ))
            }
        };
        let (eligible, index) = self.snapshot()?;
        let item = resolve(&parse_ref(token)?, &eligible.items, &index, true)?;
        Ok((item.clone(), index.get(&item.id).copied(), eligible.tab))
    }

    /// Every entry `refs` names, resolved against one snapshot so a command
    /// that removes them does not shift the entries it has not reached yet.
    /// Repeats collapse.
    fn resolve_all(
        &mut self,
        refs: &[String],
        allow_index: bool,
    ) -> Result<(Vec<ClipItem>, String), Error> {
        if refs.is_empty() {
            return Err(Error::new(Code::InvalidRef, "no clipboard entry given"));
        }
        let (eligible, index) = self.snapshot()?;
        let mut chosen: Vec<ClipItem> = Vec::new();
        for token in refs {
            let item = resolve(&parse_ref(token)?, &eligible.items, &index, allow_index)?;
            if !chosen.iter().any(|known| known.id == item.id) {
                chosen.push(item.clone());
            }
        }
        Ok((chosen, eligible.tab))
    }
}

fn execute(config: &ClipboardAddonConfig, args: &ClipboardArgs, io: Io) -> Result<Output, Error> {
    let settings = Settings::from_config(config)?;
    let backend = History::new(settings.backend).resolve()?;
    let mut run = Run {
        clips: Clips::open(backend)?,
        settings: &settings,
        clock: Clock::now(),
        io,
    };
    match &args.action {
        None => list(&mut run, &ClipboardListArgs::default()),
        Some(ClipboardCommand::List(list_args)) => list(&mut run, list_args),
        Some(ClipboardCommand::Show(show_args)) => {
            let refs = expand_refs(&show_args.reference)?;
            show(&mut run, show_args, &refs)
        }
        Some(ClipboardCommand::Copy(copy_args)) => {
            let refs = expand_refs(&copy_args.reference)?;
            copy(&mut run, &refs)
        }
        Some(ClipboardCommand::Set(set_args)) => {
            let mut input = std::io::stdin().lock();
            set(&mut run, set_args, &mut input)
        }
        Some(ClipboardCommand::Delete(delete_args)) => {
            let refs = expand_refs(&delete_args.reference)?;
            delete(&mut run, &refs)
        }
        Some(ClipboardCommand::Clear(clear_args)) => clear(&mut run, clear_args),
        Some(ClipboardCommand::Status(status_args)) => status(&mut run, status_args),
    }
}

/// `-` stands for the references a selector writes on its standard output,
/// one per line, of which only the first tab-separated field is read.
fn expand_refs(tokens: &[String]) -> Result<Vec<String>, Error> {
    let mut refs = Vec::new();
    for token in tokens {
        if token != "-" {
            refs.push(token.clone());
            continue;
        }
        for line in std::io::stdin().lock().lines() {
            let line = line.map_err(|error| {
                Error::new(Code::Io, format!("cannot read the references: {error}"))
            })?;
            let field = line.split('\t').next().unwrap_or_default().trim();
            if !field.is_empty() {
                refs.push(field.to_string());
            }
        }
    }
    Ok(refs)
}

/// A reference to one history entry.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Ref {
    /// Position in the default listing, newest first.
    Index(usize),
    /// Leading characters of an id.
    Prefix(String),
    /// One reference per line on standard input.
    Stdin,
}

fn parse_ref(token: &str) -> Result<Ref, Error> {
    if token == "-" {
        return Ok(Ref::Stdin);
    }
    if token.is_empty() {
        return Err(Error::new(
            Code::InvalidRef,
            "an empty reference names no clipboard entry",
        ));
    }
    if token.len() <= MAX_INDEX_DIGITS && token.bytes().all(|byte| byte.is_ascii_digit()) {
        let index = token.parse().map_err(|_| {
            Error::new(
                Code::InvalidRef,
                format!("`{token}` is not a clipboard index"),
            )
        })?;
        return Ok(Ref::Index(index));
    }
    if token.len() >= MIN_PREFIX && token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Ok(Ref::Prefix(token.to_ascii_lowercase()));
    }
    Err(Error::new(
        Code::InvalidRef,
        format!(
            "`{}` is not a clipboard reference: use an index of up to \
             {MAX_INDEX_DIGITS} digits, at least {MIN_PREFIX} characters of an id, \
             or `-` to read references from the input",
            sanitize(token, Sanitize::Spaces)
        ),
    ))
}

/// The entry `reference` names. Indexes are only accepted where the command
/// is harmless: they change as soon as the history does.
fn resolve<'a>(
    reference: &Ref,
    eligible: &'a [ClipItem],
    index: &HashMap<String, usize>,
    allow_index: bool,
) -> Result<&'a ClipItem, Error> {
    match reference {
        Ref::Stdin => Err(Error::new(
            Code::InvalidRef,
            "this command does not read references from the input",
        )),
        Ref::Index(position) => {
            if !allow_index {
                return Err(Error::new(
                    Code::IndexNotAllowed,
                    format!(
                        "`{position}` is an index, which changes as the history moves; \
                         pass the id `raffi clipboard list` prints"
                    ),
                ));
            }
            if *position == 0 {
                return Err(Error::new(Code::InvalidRef, "clipboard indexes start at 1"));
            }
            eligible
                .iter()
                .find(|item| index.get(&item.id) == Some(position))
                .ok_or_else(|| {
                    Error::new(
                        Code::NotFound,
                        format!(
                            "no clipboard entry at index {position} (the history lists {} entries)",
                            index.len()
                        ),
                    )
                })
        }
        Ref::Prefix(prefix) => {
            let matches: Vec<&ClipItem> = eligible
                .iter()
                .filter(|item| item.id.starts_with(prefix.as_str()))
                .collect();
            match matches.as_slice() {
                [] => Err(Error::new(
                    Code::NotFound,
                    format!("no clipboard entry matches `{prefix}`"),
                )),
                [only] => Ok(only),
                many => Err(Error::new(
                    Code::AmbiguousRef,
                    format!(
                        "`{prefix}` matches {} entries: {}",
                        many.len(),
                        candidates(many)
                    ),
                )),
            }
        }
    }
}

/// The entries an ambiguous id prefix could mean, titled so the reader can
/// tell them apart.
fn candidates(items: &[&ClipItem]) -> String {
    const SHOWN: usize = 5;
    let mut listed: Vec<String> = items
        .iter()
        .take(SHOWN)
        .map(|item| {
            format!(
                "{} ({})",
                short_id(&item.id),
                fit(&preview_of(item, Sanitize::Spaces), 40)
            )
        })
        .collect();
    if items.len() > SHOWN {
        listed.push(format!("and {} more", items.len() - SHOWN));
    }
    listed.join(", ")
}

fn short_id(id: &str) -> String {
    id.chars().take(SHORT_ID_CHARS).collect()
}

/// The SQL predicate a `-k` value stands for, `1` for every entry.
fn kind_filter(kind: Option<ClipboardKind>) -> Result<&'static str, Error> {
    match kind {
        Some(kind) => kind_predicate(kind.name()).ok_or_else(|| {
            Error::new(
                Code::InvalidArgument,
                format!("`{}` is not a clipboard kind", kind.name()),
            )
        }),
        None => Ok("1"),
    }
}

/// The desktop `Name=` behind each source, resolved the first time a row
/// needs it and kept for the rows of the same app, so a listing reads the
/// application directories at most once and only when a source is printed.
#[derive(Default)]
struct SourceNames {
    entries: Option<crate::DesktopEntries>,
    names: HashMap<String, Option<String>>,
}

impl SourceNames {
    fn name(&mut self, source: Option<&str>) -> Option<String> {
        let source = source?;
        if let Some(known) = self.names.get(source) {
            return known.clone();
        }
        let name = self
            .entries
            .get_or_insert_with(crate::read_desktop_entries)
            .resolve(source)
            .and_then(|entry| entry.name.clone())
            .filter(|name| !name.trim().is_empty());
        self.names.insert(source.to_string(), name.clone());
        name
    }
}

/// One entry as one line of output, in the shape the JSON mode prints.
#[derive(Debug, Serialize)]
struct Row {
    index: Option<usize>,
    id: String,
    kind: &'static str,
    /// 0: text starts with the query, 1: contains it, 2: contains every word.
    tier: u8,
    formats: Vec<String>,
    text_len: usize,
    copied: Option<String>,
    source: Option<String>,
    /// Name= of the desktop entry the source names, null when none does.
    source_name: Option<String>,
    expires_at: Option<i64>,
    expires_at_iso: Option<String>,
    /// Pixel size of an image entry, null for anything else.
    width: Option<u32>,
    height: Option<u32>,
    preview: String,
}

fn row(
    item: &ClipItem,
    index: Option<usize>,
    image: Option<&ImageMeta>,
    names: Option<&mut SourceNames>,
) -> Row {
    Row {
        index,
        id: item.id.clone(),
        kind: kind_name(item.kind),
        tier: item.tier,
        formats: item.formats.clone(),
        text_len: item.text_len,
        copied: item.copied.and_then(unix_rfc3339),
        source_name: names.and_then(|names| names.name(item.source.as_deref())),
        source: item.source.clone(),
        expires_at: item.expires_at,
        expires_at_iso: item.expires_at.and_then(unix_rfc3339),
        width: image.map(|meta| meta.width),
        height: image.map(|meta| meta.height),
        preview: preview_with_image(item, image, Sanitize::Spaces, false),
    }
}

/// Unix time `secs` as RFC 3339, with the UTC offset in force then. The
/// instant is what matters, so a local time the clock passed twice keeps the
/// offset it was really written with.
fn unix_rfc3339(secs: i64) -> Option<String> {
    rfc3339_at(secs, &chrono::Local)
}

fn rfc3339_at<Tz: TimeZone>(secs: i64, timezone: &Tz) -> Option<String> {
    timezone
        .timestamp_opt(secs, 0)
        .single()
        .map(|time| time.to_rfc3339_opts(chrono::SecondsFormat::Secs, false))
}

/// The entry's label on one line, without anything that would drive a
/// terminal.
fn preview_of(item: &ClipItem, mode: Sanitize) -> String {
    let title = one_line(&item.title(), PREVIEW_CHARS);
    sanitize(&title.replace(LINE_BREAK_MARK, " "), mode)
}

/// `PNG image 1920×1080` for an image whose header gave its size.
fn image_description(meta: &ImageMeta) -> String {
    let format = meta
        .mime
        .strip_prefix("image/")
        .unwrap_or(&meta.mime)
        .to_uppercase();
    format!("{format} image {}×{}", meta.width, meta.height)
}

/// The one-line label of an entry: the size of an image when its backend
/// knows it, the entry's own title otherwise. `with_size` adds how big the
/// stored picture is, which only the table has room for.
fn preview_with_image(
    item: &ClipItem,
    image: Option<&ImageMeta>,
    mode: Sanitize,
    with_size: bool,
) -> String {
    match image {
        Some(meta) if with_size => sanitize(
            &format!(
                "{} · {}",
                image_description(meta),
                format_byte_size(meta.bytes as u64)
            ),
            mode,
        ),
        Some(meta) => sanitize(&image_description(meta), mode),
        None => preview_of(item, mode),
    }
}

fn json_line<T: Serialize>(value: &T) -> Result<String, Error> {
    let mut line = serde_json::to_string(value)
        .map_err(|error| Error::new(Code::Io, format!("cannot format: {error}")))?;
    line.push('\n');
    Ok(line)
}

fn json_lines(rows: &[Row]) -> Result<String, Error> {
    rows.iter().map(json_line).collect()
}

/// One entry per line, tab separated: id, kind, copy time, preview. Stable
/// enough for a script or a picker to split on tabs.
fn tsv(rows: &[Row]) -> String {
    let mut out = String::new();
    for row in rows {
        out.push_str(&row.id);
        out.push('\t');
        out.push_str(row.kind);
        out.push('\t');
        out.push_str(row.copied.as_deref().unwrap_or_default());
        out.push('\t');
        out.push_str(&row.preview);
        out.push('\n');
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sanitize {
    /// Caret notation, for the table: an escape becomes `^[`.
    Carets,
    /// A space, for tab-separated output and messages.
    Spaces,
}

/// `text` without the characters that would drive a terminal: C0 controls and
/// DEL become caret notation or a space, C1 controls become a space, and the
/// bidi overrides that reorder text are dropped.
fn sanitize(text: &str, mode: Sanitize) -> String {
    if mode == Sanitize::Spaces {
        return crate::sanitize_display_text(text);
    }
    let mut clean = String::with_capacity(text.len());
    for character in text.chars() {
        if crate::is_bidi_override(character) {
            continue;
        }
        match character {
            '\u{0}'..='\u{1f}' | '\u{7f}' => {
                clean.push('^');
                clean.push(caret(character));
            }
            '\u{80}'..='\u{9f}' => clean.push(' '),
            _ => clean.push(character),
        }
    }
    clean
}

/// The character caret notation prints after the caret.
fn caret(control: char) -> char {
    match control {
        '\u{7f}' => '?',
        other => char::from_u32(u32::from(other) + 0x40).unwrap_or('?'),
    }
}

/// `text` cut to `budget` terminal columns, marked when something was
/// dropped. Each candidate prefix is measured as a whole with string-width
/// semantics, so a presentation sequence or a joined emoji keeps its width.
fn fit(text: &str, budget: usize) -> String {
    if text.width() <= budget {
        return text.to_string();
    }
    let mut cut = String::new();
    for character in text.chars() {
        cut.push(character);
        if cut.width() + 1 > budget {
            cut.pop();
            break;
        }
    }
    cut.push('…');
    cut
}

/// One line of the table.
struct TableRow {
    index: String,
    id: String,
    when: String,
    kind: String,
    preview: String,
}

impl TableRow {
    fn of(item: &ClipItem, index: Option<usize>, clock: Clock, image: Option<&ImageMeta>) -> Self {
        Self {
            index: index.map_or_else(|| "-".to_string(), |index| index.to_string()),
            id: short_id(&item.id),
            when: when_of(item, clock),
            kind: kind_name(item.kind).to_string(),
            preview: preview_with_image(item, image, Sanitize::Carets, true),
        }
    }
}

/// How long a temporary entry has left, at a scale that stays readable.
fn remaining(seconds: i64) -> String {
    let seconds = seconds.max(0);
    match seconds {
        0..=59 => format!("{seconds}s"),
        60..=3599 => format!("{}m", seconds / 60),
        3600..=86_399 => format!("{}h", seconds / 3600),
        _ => format!("{}d", seconds / 86_400),
    }
}

/// The copy time, with how long a temporary entry has left.
fn when_of(item: &ClipItem, clock: Clock) -> String {
    let mut when = item
        .copied
        .and_then(local_time)
        .map_or_else(String::new, |copied| short_copy_time(copied, clock.local));
    if let Some(deadline) = item.expires_at {
        if !when.is_empty() {
            when.push(' ');
        }
        when.push_str(&format!(
            "⏱ {}",
            remaining(deadline.saturating_sub(clock.secs))
        ));
    }
    when
}

fn table(rows: &[TableRow], width: usize, color: bool) -> String {
    if rows.is_empty() {
        return String::new();
    }
    let mut index_width = "#".len();
    let mut id_width = "ID".len();
    let mut when_width = "WHEN".len();
    let mut kind_width = "KIND".len();
    for row in rows {
        index_width = index_width.max(row.index.width());
        id_width = id_width.max(row.id.width());
        when_width = when_width.max(row.when.width());
        kind_width = kind_width.max(row.kind.width());
    }
    const GAP: usize = 2;
    let preview_width = width
        .saturating_sub(index_width + id_width + when_width + kind_width + GAP * 4)
        .max(MIN_PREVIEW_CHARS);
    let padded = |value: &str, column: usize| {
        let value = fit(value, column);
        let padding = column.saturating_sub(value.width());
        format!("{value}{}", " ".repeat(padding))
    };
    let faint = |value: String| {
        if color {
            format!("\x1b[2m{value}\x1b[0m")
        } else {
            value
        }
    };
    let line = |index: String, id: String, when: String, kind: String, preview: String| {
        format!("{index}  {id}  {when}  {kind}  {preview}\n")
    };
    let mut out = line(
        faint(padded("#", index_width)),
        faint(padded("ID", id_width)),
        padded("WHEN", when_width),
        padded("KIND", kind_width),
        "PREVIEW".to_string(),
    );
    for row in rows {
        out.push_str(&line(
            faint(padded(&row.index, index_width)),
            faint(padded(&row.id, id_width)),
            padded(&row.when, when_width),
            padded(&row.kind, kind_width),
            fit(&row.preview, preview_width),
        ));
    }
    out
}

/// The rows of a listing, each with its position in the default listing. A
/// row whose id is not in the snapshot is dropped.
fn selected<'a>(
    page: &'a Page,
    eligible_ids: &HashSet<String>,
    index: &HashMap<String, usize>,
) -> Vec<(&'a ClipItem, Option<usize>)> {
    page.items
        .iter()
        .filter(|item| eligible_ids.contains(&item.id))
        .map(|item| (item, index.get(&item.id).copied()))
        .collect()
}

/// The rows a listing prints: those copied at or after `since`, then the
/// first `offset` dropped, then cut to `limit`. The position of a row stays
/// the one it has in the whole snapshot.
fn window(
    items: Vec<(&ClipItem, Option<usize>)>,
    since: Option<i64>,
    offset: usize,
    limit: Option<usize>,
) -> Vec<(&ClipItem, Option<usize>)> {
    let mut items = items;
    if let Some(cutoff) = since {
        // A copy time is what `--since` filters on, so an entry without one
        // cannot tell whether it belongs.
        items.retain(|(item, _)| item.copied.is_some_and(|copied| copied >= cutoff));
    }
    if offset > 0 {
        items.drain(..offset.min(items.len()));
    }
    if let Some(limit) = limit {
        items.truncate(limit);
    }
    items
}

/// The instant `--since` starts at, in unix seconds: a duration back from
/// now, an RFC 3339 timestamp, or a `YYYY-MM-DD` date at local midnight.
fn parse_since(value: &str, now: i64) -> Result<i64, Error> {
    let value = value.trim();
    if let Some(secs) = duration_secs(value) {
        return Ok(now.saturating_sub(secs));
    }
    if let Ok(time) = chrono::DateTime::parse_from_rfc3339(value) {
        return Ok(time.timestamp());
    }
    if let Ok(date) = chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d") {
        if let Some(midnight) = date.and_hms_opt(0, 0, 0) {
            return Ok(local_midnight_unix(midnight));
        }
    }
    Err(Error::usage(format!(
        "`{}` is not a duration (30s, 15m, 2h, 3d) or a timestamp \
         (RFC 3339, or YYYY-MM-DD)",
        sanitize(value, Sanitize::Spaces)
    )))
}

/// The unix instant of a local midnight, which a timezone with a
/// daylight-saving change at midnight may not have: the first minute of the
/// day that exists starts it then.
fn local_midnight_unix(midnight: NaiveDateTime) -> i64 {
    day_start_unix(&chrono::Local, midnight)
}

fn day_start_unix<Tz: TimeZone>(zone: &Tz, midnight: NaiveDateTime) -> i64 {
    (0..24 * 60)
        .map(|minutes| midnight + chrono::Duration::minutes(minutes))
        .find_map(|time| zone.from_local_datetime(&time).earliest())
        .map_or_else(|| midnight.and_utc().timestamp(), |time| time.timestamp())
}

/// How many seconds `30s`, `15m`, `2h` or `3d` stands for.
fn duration_secs(value: &str) -> Option<i64> {
    // The unit is one ASCII letter, so a value ending in a multi-byte
    // character is not a duration rather than a panic.
    let unit = value.chars().next_back()?;
    let multiplier = match unit {
        's' => 1,
        'm' => 60,
        'h' => 60 * 60,
        'd' => 24 * 60 * 60,
        _ => return None,
    };
    let number = value.get(..value.len() - 1)?;
    if number.is_empty() || !number.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    number
        .parse::<i64>()
        .ok()
        .map(|number| number.saturating_mul(multiplier))
}

fn list(run: &mut Run, args: &ClipboardListArgs) -> Result<Output, Error> {
    let query = args.query.join(" ");
    let min_chars = if args.all { 0 } else { run.settings.min_chars };
    let (eligible, index) = run.snapshot()?;
    let eligible_ids: HashSet<String> = eligible.items.iter().map(|item| item.id.clone()).collect();
    let page = if query.trim().is_empty() && args.kind.is_none() && args.all {
        eligible
    } else {
        run.search(&query, args.kind, min_chars)?
    };
    // A search lists again, so drop anything recorded after the snapshot: an
    // entry outside it has no position and must not appear as an extra row.
    let items = selected(&page, &eligible_ids, &index);
    let since = match &args.since {
        Some(value) => Some(parse_since(value, run.clock.secs)?),
        None => None,
    };
    let items = window(items, since, args.offset.unwrap_or(0), args.limit);
    // Only the rows that survived the window need their picture sized.
    let now = run.clock.secs;
    let images: HashMap<String, ImageMeta> = items
        .iter()
        .filter_map(|(item, _)| {
            run.clips
                .image_meta(item, now)
                .map(|meta| (item.id.clone(), meta))
        })
        .collect();
    let image_of = |item: &ClipItem| images.get(&item.id);
    if !args.json && run.io.stdout_tty {
        let rows: Vec<TableRow> = items
            .iter()
            .map(|(item, index)| TableRow::of(item, *index, run.clock, image_of(item)))
            .collect();
        return Ok(Output::text(table(&rows, run.io.width, run.io.color)));
    }
    // Only the JSON rows carry a source name: the table and the
    // tab-separated lines skip the desktop entry lookup.
    let mut sources = SourceNames::default();
    let rows: Vec<Row> = items
        .iter()
        .map(|(item, index)| {
            let sources = args.json.then_some(&mut sources);
            row(item, *index, image_of(item), sources)
        })
        .collect();
    Ok(Output::text(if args.json {
        json_lines(&rows)?
    } else {
        tsv(&rows)
    }))
}

fn show(run: &mut Run, args: &ClipboardShowArgs, refs: &[String]) -> Result<Output, Error> {
    let (item, index, tab) = run.resolve_one(refs)?;
    let mut sources = SourceNames::default();
    match &run.clips {
        Clips::CopyQ(copyq) => show_copyq(copyq, &item, &tab, args, index, &mut sources),
        Clips::Raffi(Some(store)) => show_raffi(store, &item, args, run, index, &mut sources),
        Clips::Raffi(None) => Err(Error::new(Code::NoHistory, "no clipboard history")),
    }
}

fn show_raffi(
    store: &Store,
    item: &ClipItem,
    args: &ClipboardShowArgs,
    run: &Run,
    index: Option<usize>,
    sources: &mut SourceNames,
) -> Result<Output, Error> {
    let formats = store.formats(&item.id, run.clock.secs)?;
    if args.formats {
        return formats_output(&sizes_of(&formats), args.json);
    }
    if let Some(mime) = &args.mime {
        return match formats.iter().find(|(name, _)| name == mime) {
            Some((_, bytes)) => Ok(Capped::raw(bytes.clone(), args.max_bytes).output()),
            None => Err(Error::new(
                Code::NotFound,
                format!(
                    "the entry holds no {} format, only {}",
                    sanitize(mime, Sanitize::Spaces),
                    names(&formats)
                ),
            )),
        };
    }
    if args.json {
        let text = match item.kind {
            ItemKind::Image => None,
            _ => exact_text(&formats).map(|bytes| Capped::text(bytes, args.max_bytes)),
        };
        let image = store.image_meta(&item.id, run.clock.secs).ok().flatten();
        return metadata_line(item, index, text, image.as_ref(), sources);
    }
    match item.kind {
        ItemKind::Image => {
            let (mime, bytes) = preferred_image(&formats)
                .ok_or_else(|| Error::new(Code::NotFound, "the entry holds no image format"))?;
            if run.io.stdout_tty {
                return Err(Error::new(
                    Code::Unsupported,
                    format!(
                        "entry {} is a {} image ({}); redirect its output or pass --mime",
                        index.map_or_else(|| short_id(&item.id), |index| index.to_string()),
                        sanitize(&mime["image/".len()..], Sanitize::Spaces).to_uppercase(),
                        format_byte_size(bytes.len() as u64)
                    ),
                ));
            }
            Ok(Capped::raw(bytes, args.max_bytes).output())
        }
        _ => exact_text(&formats)
            .map(|bytes| Capped::text(bytes, args.max_bytes).output())
            .ok_or_else(|| {
                Error::new(
                    Code::NotFound,
                    format!("the entry holds no text to show, only {}", names(&formats)),
                )
            }),
    }
}

fn show_copyq(
    copyq: &CopyQ,
    item: &ClipItem,
    tab: &str,
    args: &ClipboardShowArgs,
    index: Option<usize>,
    sources: &mut SourceNames,
) -> Result<Output, Error> {
    if args.mime.is_some() {
        return Err(Error::new(
            Code::Unsupported,
            "--mime is not supported by the copyq backend",
        ));
    }
    if args.formats {
        let preview = copyq.preview(&item.reference(tab))?;
        return formats_output(&preview.formats, args.json);
    }
    if args.json {
        let text = match item.kind {
            ItemKind::Image => None,
            _ => Some(Capped::text(
                copyq
                    .full_text(&item.reference(tab))
                    .map_err(|error| copyq_text_error(&error))?
                    .into_bytes(),
                args.max_bytes,
            )),
        };
        return metadata_line(item, index, text, None, sources);
    }
    if item.kind == ItemKind::Image {
        return Err(Error::new(
            Code::Unsupported,
            "exporting an image is not supported by the copyq backend",
        ));
    }
    let text = copyq
        .full_text(&item.reference(tab))
        .map_err(|error| copyq_text_error(&error))?;
    Ok(Capped::text(text.into_bytes(), args.max_bytes).output())
}

fn copyq_text_error(error: &str) -> Error {
    if error.contains("longer than") {
        return Error::new(
            Code::TooLarge,
            format!(
                "the entry is larger than {}; export it with CopyQ",
                format_byte_size(clipboard_store::MAX_TEXT_BYTES as u64)
            ),
        );
    }
    Error::from(error.to_owned())
}

/// The formats of an entry with their size, for `show --formats`.
fn formats_output(formats: &[(String, usize)], json: bool) -> Result<Output, Error> {
    #[derive(Serialize)]
    struct Entry<'a> {
        mime: &'a str,
        bytes: usize,
    }
    #[derive(Serialize)]
    struct Listing<'a> {
        formats: Vec<Entry<'a>>,
    }
    if json {
        let listing = Listing {
            formats: formats
                .iter()
                .map(|(mime, bytes)| Entry {
                    mime,
                    bytes: *bytes,
                })
                .collect(),
        };
        return Ok(Output::text(json_line(&listing)?));
    }
    // Only the JSON listing keeps the exact stored MIME strings; the table a
    // terminal shows cannot let them drive the screen.
    let labels: Vec<String> = formats
        .iter()
        .map(|(mime, _)| sanitize(mime, Sanitize::Spaces))
        .collect();
    let width = labels.iter().map(|label| label.width()).max().unwrap_or(0);
    let mut out = String::new();
    for ((_, bytes), label) in formats.iter().zip(&labels) {
        out.push_str(&format!(
            "{}  {}\n",
            fit(label, width),
            format_byte_size(*bytes as u64)
        ));
    }
    Ok(Output::text(out))
}

/// The entry's JSON row, with its `text` when it has one: null for an image,
/// which has no text to show.
fn metadata_line(
    item: &ClipItem,
    index: Option<usize>,
    text: Option<Capped>,
    image: Option<&ImageMeta>,
    sources: &mut SourceNames,
) -> Result<Output, Error> {
    let mut value = serde_json::to_value(row(item, index, image, Some(sources)))
        .map_err(|error| Error::new(Code::Io, format!("cannot format the entry: {error}")))?;
    let truncated = text.as_ref().is_some_and(|capped| capped.truncated);
    value["text"] = match text {
        Some(capped) => String::from_utf8_lossy(&capped.bytes).into_owned().into(),
        None => serde_json::Value::Null,
    };
    value["truncated"] = truncated.into();
    Ok(Output::text(json_line(&value)?))
}

fn sizes_of(formats: &[(String, Vec<u8>)]) -> Vec<(String, usize)> {
    formats
        .iter()
        .map(|(mime, bytes)| (mime.clone(), bytes.len()))
        .collect()
}

/// The bytes one `show` writes, cut to `--max-bytes` when the entry is
/// larger.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Capped {
    bytes: Vec<u8>,
    /// Length of the content before the cut.
    len: usize,
    truncated: bool,
}

impl Capped {
    /// `bytes`, cut to `max` bytes of text, keeping whole characters.
    fn text(bytes: Vec<u8>, max: Option<usize>) -> Self {
        Self::cut(bytes, max, true)
    }

    /// `bytes`, cut to `max` bytes whatever they hold.
    fn raw(bytes: Vec<u8>, max: Option<usize>) -> Self {
        Self::cut(bytes, max, false)
    }

    fn cut(bytes: Vec<u8>, max: Option<usize>, text: bool) -> Self {
        let len = bytes.len();
        match max {
            Some(max) if len > max => {
                let end = if text {
                    char_boundary(&bytes, max)
                } else {
                    max
                };
                Self {
                    bytes: bytes[..end].to_vec(),
                    len,
                    truncated: end < len,
                }
            }
            _ => Self {
                bytes,
                len,
                truncated: false,
            },
        }
    }

    fn output(self) -> Output {
        let written = self.bytes.len();
        let output = Output::bytes(self.bytes);
        if self.truncated {
            return output.notice(format!(
                "raffi clipboard: output truncated to {written} of {} bytes",
                self.len
            ));
        }
        output
    }
}

/// The largest length at most `max` that does not split a UTF-8 character of
/// `bytes`.
fn char_boundary(bytes: &[u8], max: usize) -> usize {
    if max >= bytes.len() {
        return bytes.len();
    }
    let mut end = max;
    while end > 0 && (bytes[end] & 0b1100_0000) == 0b1000_0000 {
        end -= 1;
    }
    end
}

/// The bytes `show` writes when no format is asked for, exactly as stored:
/// plain text first, then the file list decoded to one path per line, then
/// the text extracted from the HTML.
fn exact_text(formats: &[(String, Vec<u8>)]) -> Option<Vec<u8>> {
    if let Some((_, bytes)) = formats
        .iter()
        .find(|(mime, _)| TEXT_FORMATS.contains(&mime.as_str()))
    {
        return Some(bytes.clone());
    }
    if let Some((_, bytes)) = formats.iter().find(|(mime, _)| mime == "text/uri-list") {
        let list = String::from_utf8_lossy(bytes);
        return Some(uri_lines_text(&list).into_bytes());
    }
    formats
        .iter()
        .find(|(mime, _)| mime == "text/html")
        .map(|_| clipboard_store::text_of(formats).into_bytes())
}

fn uri_lines_text(list: &str) -> String {
    let mut out = String::new();
    for entry in parse_uri_list(list, usize::MAX) {
        out.push_str(&entry);
        out.push('\n');
    }
    out
}

/// The image format exported by default: PNG first, then a raster format, SVG
/// last since a viewer may not open it.
fn preferred_image(formats: &[(String, Vec<u8>)]) -> Option<(String, Vec<u8>)> {
    let images = || {
        formats
            .iter()
            .filter(|(mime, _)| mime.starts_with("image/"))
    };
    images()
        .find(|(mime, _)| mime == "image/png")
        .or_else(|| images().find(|(mime, _)| mime != "image/svg+xml"))
        .or_else(|| images().next())
        .cloned()
}

fn names(formats: &[(String, Vec<u8>)]) -> String {
    if formats.is_empty() {
        return "no format".to_string();
    }
    formats
        .iter()
        .map(|(mime, _)| sanitize(mime, Sanitize::Spaces))
        .collect::<Vec<_>>()
        .join(", ")
}

fn copy(run: &mut Run, refs: &[String]) -> Result<Output, Error> {
    let (item, _, tab) = run.resolve_one(refs)?;
    match &run.clips {
        Clips::CopyQ(copyq) => copyq.select(&item.reference(&tab))?,
        Clips::Raffi(_) => super::cache::clipboard_select(&item.id)?,
    }
    Ok(Output::default())
}

/// Put new content on the clipboard, recording it in the history the way a
/// copy from another app would be.
fn set(run: &mut Run, args: &ClipboardSetArgs, input: &mut dyn Read) -> Result<Output, Error> {
    let mime = canonical_mime(args.mime.as_deref().unwrap_or(DEFAULT_MIME))?;
    let source = source_app(args.source.as_deref(), &crate::desktop_entry_dirs())?;
    let data = content(&mime, args, run.io, &run.settings.limits, input)?;
    check_content(&mime, &data, &run.settings.limits)?;
    match &run.clips {
        Clips::Raffi(_) => {
            let outcome =
                super::cache::clipboard_set(&[(mime, data)], args.temporary, args.secret, source)?;
            let recorded = outcome.id.is_some();
            set_output(
                outcome.id.as_deref(),
                recorded,
                outcome.expires_at,
                args.json,
            )
        }
        Clips::CopyQ(copyq) => set_copyq(copyq, &mime, &data, args, source),
    }
}

/// The app name `--source` records, checked before it reaches the history.
///
/// A value ending in `.desktop` is a desktop entry ID: it has to be the ID of
/// an entry of `dirs`, and is recorded without that suffix so it matches the
/// app_id a compositor reports. Anything else is a free label, kept as it
/// came.
fn source_app(source: Option<&str>, dirs: &[std::path::PathBuf]) -> Result<Option<String>, Error> {
    let Some(source) = source else {
        return Ok(None);
    };
    let source = clipboard_store::validate_source(source).map_err(Error::usage)?;
    if source.contains(['/', '\\']) || source.starts_with('~') {
        return Err(Error::usage(format!(
            "`{source}` is a path; put the file in ~/.local/share/applications and pass its desktop entry ID instead"
        )));
    }
    if !source.to_lowercase().ends_with(".desktop") {
        return Ok(Some(source));
    }
    match crate::DesktopEntries::from_dirs(dirs).resolve_id(&source) {
        Some(entry) => Ok(Some(entry.id.clone())),
        None => Err(Error::usage(format!(
            "no desktop entry `{source}` in {}",
            dirs.iter()
                .map(|dir| dir.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

fn set_copyq(
    copyq: &CopyQ,
    mime: &str,
    data: &[u8],
    args: &ClipboardSetArgs,
    source: Option<String>,
) -> Result<Output, Error> {
    if args.temporary {
        return Err(Error::new(
            Code::Unsupported,
            "--temporary is not supported by the copyq backend",
        ));
    }
    if args.secret {
        return Err(Error::new(
            Code::Unsupported,
            "--secret is not supported by the copyq backend",
        ));
    }
    if source.is_some() {
        return Err(Error::new(
            Code::Unsupported,
            "--source is not supported by the copyq backend",
        ));
    }
    copyq.set(mime, data)?;
    // CopyQ keeps the content in its own history, but does not hand out the
    // id it gave it.
    set_output(None, true, None, args.json)
}

/// What `set` writes: the id of the entry it recorded, and nothing at all
/// when the content stayed out of the history.
#[derive(Debug, Serialize)]
struct SetReport<'a> {
    id: Option<&'a str>,
    recorded: bool,
    expires_at: Option<i64>,
    expires_at_iso: Option<String>,
}

fn set_output(
    id: Option<&str>,
    recorded: bool,
    expires_at: Option<i64>,
    json: bool,
) -> Result<Output, Error> {
    if json {
        let report = SetReport {
            id,
            recorded,
            expires_at,
            expires_at_iso: expires_at.and_then(unix_rfc3339),
        };
        return Ok(Output::text(json_line(&report)?));
    }
    Ok(match id {
        Some(id) => Output::text(format!("{id}\n")),
        None => Output::default(),
    })
}

/// The MIME type the content is stored under: a plain text type is stored as
/// `text/plain`, which the selection then offers under its usual aliases, and
/// any other recognized text type loses its parameters, so the recorder
/// extracts its text and applies its policy to it.
fn canonical_mime(mime: &str) -> Result<String, Error> {
    let mime = mime.trim();
    if mime.is_empty() {
        return Err(Error::new(
            Code::InvalidArgument,
            "an empty MIME type names no content",
        ));
    }
    if mime.chars().any(char::is_control) || mime.len() > clipboard_store::MAX_MIME_BYTES {
        return Err(Error::new(
            Code::InvalidArgument,
            format!(
                "`{}` is not a MIME type: it has a control character or is too long",
                sanitize(mime, Sanitize::Spaces)
            ),
        ));
    }
    // The names a selection offers plain text under are not MIME types, but
    // they are accepted here and stored as the one type they stand for.
    if is_plain_text(mime) {
        return Ok(DEFAULT_MIME.to_string());
    }
    clipboard_store::validate_mime(mime).map_err(|reason| {
        Error::new(
            Code::InvalidArgument,
            format!(
                "`{}` is not a MIME type: {reason}",
                sanitize(mime, Sanitize::Spaces)
            ),
        )
    })?;
    Ok(clipboard_store::canonical_mime(mime))
}

/// Whether `mime` is text as far as a Wayland client is concerned: a `text/`
/// type, or one of the names plain text is offered under.
fn is_text(mime: &str) -> bool {
    mime.to_ascii_lowercase().starts_with("text/") || is_plain_text(mime)
}

fn is_plain_text(mime: &str) -> bool {
    let mime = mime.trim().to_ascii_lowercase();
    mime == "text/plain"
        || mime.starts_with("text/plain;")
        || matches!(mime.as_str(), "utf8_string" | "string" | "text")
}

/// The bytes `set` puts on the clipboard: the arguments joined by single
/// spaces, or everything its input holds. A terminal is never read for that,
/// since it would wait for the reader to type.
fn content(
    mime: &str,
    args: &ClipboardSetArgs,
    io: Io,
    limits: &clipboard_store::Limits,
    input: &mut dyn Read,
) -> Result<Vec<u8>, Error> {
    if !args.text.is_empty() {
        return Ok(args.text.join(" ").into_bytes());
    }
    if io.stdin_tty {
        return Err(Error::new(
            Code::InvalidArgument,
            "no content: pass it as arguments or pipe it to the input",
        ));
    }
    // One byte over the limit is enough to tell the content is too large,
    // without reading all of a pipe that may never end.
    let mut data = Vec::new();
    (&mut *input)
        .take(format_limit(mime, limits).saturating_add(1))
        .read_to_end(&mut data)
        .map_err(|error| Error::new(Code::Io, format!("cannot read the input: {error}")))?;
    Ok(data)
}

/// The size a format of `mime` may reach, the same the recorder applies.
fn format_limit(mime: &str, limits: &clipboard_store::Limits) -> u64 {
    if mime.starts_with("image/") {
        limits.max_image_size
    } else {
        clipboard_store::MAX_TEXT_BYTES as u64
    }
}

fn check_content(mime: &str, data: &[u8], limits: &clipboard_store::Limits) -> Result<(), Error> {
    if data.is_empty() {
        return Err(Error::new(Code::InvalidArgument, "the content is empty"));
    }
    let limit = format_limit(mime, limits);
    if data.len() as u64 > limit {
        return Err(Error::new(
            Code::TooLarge,
            format!(
                "the content is {}, over the {} limit for {}",
                format_byte_size(data.len() as u64),
                format_byte_size(limit),
                sanitize(mime, Sanitize::Spaces)
            ),
        ));
    }
    if is_text(mime) && std::str::from_utf8(data).is_err() {
        return Err(Error::new(
            Code::InvalidArgument,
            "the content is not UTF-8 text; pass --mime with the type of the data",
        ));
    }
    Ok(())
}

fn delete(run: &mut Run, refs: &[String]) -> Result<Output, Error> {
    let (items, tab) = run.resolve_all(refs, false)?;
    let count = items.len();
    match &mut run.clips {
        Clips::CopyQ(copyq) => {
            for item in &items {
                copyq.delete(&item.reference(&tab))?;
            }
            Ok(deleted(count, run.io, true))
        }
        Clips::Raffi(None) => Err(Error::new(Code::NoHistory, "no clipboard history")),
        Clips::Raffi(Some(store)) => {
            let mut emptied = true;
            for item in &items {
                emptied &= store.delete(&item.id)?;
            }
            let emptied = emptied || checkpoint(store);
            Ok(deleted(count, run.io, emptied))
        }
    }
}

/// Retry the log checkpoint of a deletion that could not empty it, because
/// another connection was reading at the time. Reports whether it worked.
fn checkpoint(store: &Store) -> bool {
    for _ in 0..CHECKPOINT_RETRIES {
        std::thread::sleep(CHECKPOINT_RETRY_DELAY);
        if store.truncate_wal() {
            return true;
        }
    }
    false
}

/// What a deletion says: the count for a reader, and always when the old
/// pages are still in the log.
fn deleted(count: usize, io: Io, emptied: bool) -> Output {
    let mut output = Output::default();
    if io.stderr_tty {
        output = output.notice(match count {
            1 => "Deleted 1 entry".to_string(),
            count => format!("Deleted {count} entries"),
        });
    }
    if !emptied {
        output = output
            .notice("deleted; the database log still holds old pages until the next checkpoint");
    }
    output
}

fn clear(run: &mut Run, args: &ClipboardClearArgs) -> Result<Output, Error> {
    if run.clips.is_copyq() {
        return Err(Error::new(
            Code::Unsupported,
            "clear is only supported by the raffi backend; use CopyQ's own interface",
        ));
    }
    let filter = kind_filter(args.kind)?;
    let eligible = run.search("", args.kind, 0)?;
    let (total, alive) = match &run.clips {
        Clips::Raffi(Some(store)) => store.count(filter, run.clock.secs)?,
        _ => return Err(Error::new(Code::NoHistory, "no clipboard history")),
    };
    if total == 0 {
        return Ok(Output::default());
    }
    let hidden = alive.saturating_sub(eligible.items.len());
    let entries = match total {
        1 => "1 clipboard entry".to_string(),
        total => format!("{total} clipboard entries"),
    };
    let prompt = if hidden > 0 {
        format!("Delete {entries}, {hidden} hidden by exclude_patterns?")
    } else {
        format!("Delete {entries}?")
    };
    if !args.yes {
        confirm(&prompt, run.io)?;
    }
    let (removed, emptied) = match &mut run.clips {
        Clips::Raffi(Some(store)) => {
            let (removed, emptied) = store.clear(filter)?;
            (removed, emptied || checkpoint(store))
        }
        _ => return Err(Error::new(Code::NoHistory, "no clipboard history")),
    };
    Ok(deleted(removed, run.io, emptied))
}

/// Ask before deleting everything. Only a terminal can answer, so a script
/// has to pass `--yes` and be explicit about it.
fn confirm(prompt: &str, io: Io) -> Result<(), Error> {
    if !io.stdin_tty {
        return Err(Error::new(
            Code::InvalidArgument,
            "clear needs --yes when its input is not a terminal",
        ));
    }
    eprint!("{prompt} [y/N] ");
    std::io::stderr()
        .flush()
        .map_err(|error| Error::new(Code::Io, format!("cannot write to stderr: {error}")))?;
    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .map_err(|error| Error::new(Code::Io, format!("cannot read the answer: {error}")))?;
    if matches!(answer.trim(), "y" | "Y" | "yes" | "Yes" | "YES") {
        Ok(())
    } else {
        Err(Error::new(Code::Other, "nothing was deleted"))
    }
}

fn status(run: &mut Run, args: &ClipboardStatusArgs) -> Result<Output, Error> {
    // Only a running cache service speaks the daemon protocol, so only the
    // raffi backend can report a revision.
    let (recorder, protocol) = match &run.clips {
        Clips::Raffi(_) => match super::cache::clipboard_report() {
            Some((recording, protocol)) => (recording, Some(protocol)),
            None => (false, None),
        },
        Clips::CopyQ(_) => (
            super::clipboard_history::copyq_running(super::clipboard_history::running_cmdlines),
            None,
        ),
    };
    // CopyQ's own history count, without this installation's exclusion
    // rules, so it means the same thing as the raffi backend's count.
    let copyq_entries = if run.clips.is_copyq() {
        let now = run.clock.secs;
        Some(run.clips.list("", None, 0, &[], now)?.items.len())
    } else {
        None
    };
    let (counts, database, database_bytes) = match &run.clips {
        Clips::Raffi(store) => {
            let counts = match store {
                Some(store) => store.counts(run.clock.secs)?,
                None => clipboard_store::Counts::default(),
            };
            let database = clipboard_store::default_path()
                .ok()
                .map(|path| path.display().to_string());
            (counts, database, Some(clipboard_store::disk_usage()))
        }
        // CopyQ owns its own storage, so its count says nothing about this
        // installation's database and its size is unavailable here.
        Clips::CopyQ(_) => (
            clipboard_store::Counts {
                alive: copyq_entries.unwrap_or(0),
                ..clipboard_store::Counts::default()
            },
            None,
            None,
        ),
    };
    let status = Status {
        backend: run.clips.name(),
        configured: match run.settings.backend {
            crate::ClipboardBackend::Auto => "auto",
            crate::ClipboardBackend::Copyq => "copyq",
            crate::ClipboardBackend::Raffi => "raffi",
        },
        recorder,
        protocol,
        database,
        database_bytes,
        entries: counts.alive,
        temporary: counts.temporary,
        limits: run.settings.limits.into(),
    };
    if args.json {
        return Ok(Output::text(json_line(&status)?));
    }
    Ok(Output::text(status_text(&status)))
}

/// What `status` reports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct Status {
    /// The backend the history is read from.
    backend: &'static str,
    /// The `addons.clipboard.backend` setting that chose it.
    configured: &'static str,
    /// Whether a service records the clipboard for that backend.
    recorder: bool,
    /// Revision of the daemon protocol, when a cache service answers it.
    protocol: Option<u32>,
    /// Path of the raffi database, absent for CopyQ.
    database: Option<String>,
    /// Bytes that database takes, unknown for a backend that keeps its own.
    database_bytes: Option<u64>,
    /// Entries in the history.
    entries: usize,
    /// Entries that leave the history when their deadline passes.
    temporary: usize,
    #[serde(flatten)]
    limits: LimitsReport,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
struct LimitsReport {
    max_items: usize,
    max_size_bytes: u64,
    max_image_size_bytes: u64,
}

impl From<clipboard_store::Limits> for LimitsReport {
    fn from(limits: clipboard_store::Limits) -> Self {
        Self {
            max_items: limits.max_items,
            max_size_bytes: limits.max_size,
            max_image_size_bytes: limits.max_image_size,
        }
    }
}

fn status_text(status: &Status) -> String {
    let service = if status.backend == "raffi" {
        "raffi --cache-daemon"
    } else {
        "copyq"
    };
    let recorder = if status.recorder {
        format!("running ({service})")
    } else {
        "not running".to_string()
    };
    let database = match (&status.database, status.database_bytes) {
        (Some(path), Some(bytes)) => {
            format!("{path} ({})", format_byte_size(bytes))
        }
        (Some(path), None) => path.clone(),
        (None, _) => "not used by this backend".to_string(),
    };
    let entries = match status.temporary {
        0 => status.entries.to_string(),
        temporary => format!("{} ({temporary} temporary)", status.entries),
    };
    let backend = match status.configured {
        "auto" => format!("{} (auto)", status.backend),
        _ => status.backend.to_string(),
    };
    report(&[
        ("Backend", backend),
        ("Recorder", recorder),
        ("Database", database),
        ("Entries", entries),
        ("Limits", limits_line(&status.limits)),
    ])
}

fn limits_line(limits: &LimitsReport) -> String {
    const MIB: u64 = 1024 * 1024;
    format!(
        "{} entries, {} MB, images ≤ {} MB",
        limits.max_items,
        limits.max_size_bytes / MIB,
        limits.max_image_size_bytes / MIB
    )
}

/// Aligned labels and values, for the human reading a terminal.
fn report(rows: &[(&str, String)]) -> String {
    let width = rows
        .iter()
        .map(|(label, _)| label.width())
        .max()
        .unwrap_or(0)
        + 4;
    let mut out = String::new();
    for (label, value) in rows {
        let label = sanitize(label, Sanitize::Spaces);
        let padding = width.saturating_sub(label.width());
        let value = sanitize(value, Sanitize::Spaces);
        out.push_str(&format!("{label}{}{value}\n", " ".repeat(padding)));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::clipboard_store::{Capture, Limits};
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    const NOW: i64 = 1_700_000_000;

    const LIMITS: Limits = Limits {
        max_items: 100,
        max_size: 1 << 30,
        max_image_size: 1 << 20,
    };

    /// Positions of every entry, for the tests that only resolve references.
    fn index_of(items: &[ClipItem]) -> HashMap<String, usize> {
        items
            .iter()
            .enumerate()
            .map(|(position, item)| (item.id.clone(), position + 1))
            .collect()
    }

    fn temp_store(name: &str) -> (Store, std::path::PathBuf) {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("raffi-clipcli-{name}-{unique}"));
        let store = Store::open(&dir.join("clipboard.db")).unwrap();
        (store, dir)
    }

    fn capture(text: &str, at: i64) -> Capture {
        Capture {
            formats: vec![("text/plain".into(), text.as_bytes().to_vec())],
            copied_at: at,
            source: Some("kitty".into()),
            password_marked: false,
        }
    }

    fn settings(min_chars: usize, exclude: Vec<regex::Regex>) -> Settings {
        Settings {
            min_chars,
            exclude,
            limits: LIMITS,
            backend: crate::ClipboardBackend::Raffi,
        }
    }

    fn run(store: Store, settings: &Settings) -> Run<'_> {
        Run {
            clips: Clips::Raffi(Some(store)),
            settings,
            clock: Clock::at(NOW),
            io: Io::pipe(),
        }
    }

    fn item(id: &str, kind: ItemKind, excerpt: &str) -> ClipItem {
        ClipItem {
            row: 0,
            id: id.into(),
            formats: vec!["text/plain".into()],
            kind,
            text_len: excerpt.encode_utf16().count(),
            excerpt: excerpt.into(),
            tier: 0,
            copied: None,
            source: None,
            oversized: false,
            expires_at: None,
        }
    }

    #[test]
    fn test_references_are_indexes_id_prefixes_or_stdin() {
        assert_eq!(parse_ref("1"), Ok(Ref::Index(1)));
        assert_eq!(parse_ref("12345"), Ok(Ref::Index(12345)));
        assert_eq!(parse_ref("-"), Ok(Ref::Stdin));
        assert_eq!(parse_ref("3fa9c1"), Ok(Ref::Prefix("3fa9c1".into())));
        assert_eq!(
            parse_ref("3FA9C1D2E0"),
            Ok(Ref::Prefix("3fa9c1d2e0".into()))
        );
        assert_eq!(
            parse_ref(&"a".repeat(64)),
            Ok(Ref::Prefix("a".repeat(64))),
            "a CopyQ id is a valid reference"
        );
        // Six digits are an id prefix, not an index.
        assert_eq!(parse_ref("123456"), Ok(Ref::Prefix("123456".into())));
        // Five characters with a letter are neither.
        assert!(parse_ref("3fa9c").is_err());
        assert!(parse_ref("3fa9cq").is_err());
        assert!(parse_ref("").is_err());
        assert!(parse_ref("  ").is_err());
        assert!(parse_ref("12 3").is_err());
    }

    #[test]
    fn test_indexes_are_only_accepted_where_they_cannot_go_stale() {
        let entries = [item("a1b2c3d4e5", ItemKind::Text, "newest")];
        let index = index_of(&entries);
        let newest = Ref::Index(1);
        assert_eq!(
            resolve(&newest, &entries, &index, true).unwrap().id,
            "a1b2c3d4e5"
        );
        let refused = resolve(&newest, &entries, &index, false)
            .unwrap_err()
            .message;
        assert!(
            refused.contains("changes as the history moves"),
            "{refused}"
        );
        assert!(resolve(&Ref::Index(0), &entries, &index, true)
            .unwrap_err()
            .message
            .contains("start at 1"));
        assert!(resolve(&Ref::Index(2), &entries, &index, true)
            .unwrap_err()
            .message
            .contains("no clipboard entry at index 2"));
    }

    #[test]
    fn test_an_ambiguous_prefix_names_its_candidates() {
        let entries = [
            item(
                "3fa9c1d2e0",
                ItemKind::Text,
                "first \u{1b}]52;c;ZXZpbA==\u{7}",
            ),
            item("3fa9c1ffff", ItemKind::Image, ""),
            item("77aa01fe3c", ItemKind::Text, "third"),
        ];
        let index = index_of(&entries);
        let one = resolve(&Ref::Prefix("77aa".into()), &entries, &index, true).unwrap();
        assert_eq!(one.id, "77aa01fe3c");
        let ambiguous = resolve(&Ref::Prefix("3fa9c1".into()), &entries, &index, true)
            .unwrap_err()
            .message;
        assert!(ambiguous.contains("matches 2 entries"), "{ambiguous}");
        assert!(ambiguous.contains("3fa9c1d2e0"), "{ambiguous}");
        // The candidate titles cannot drive the terminal.
        assert!(!ambiguous.contains('\u{1b}'), "{ambiguous}");
        assert!(ambiguous.contains("]52;c;"), "{ambiguous}");
        assert!(
            resolve(&Ref::Prefix("deadbeef".into()), &entries, &index, true)
                .unwrap_err()
                .message
                .contains("no clipboard entry matches `deadbeef`")
        );
    }

    #[test]
    fn test_the_default_listing_decides_who_is_indexed() {
        let (mut store, dir) = temp_store("minchars");
        let short = store.insert(capture("ok", 1), &LIMITS, NOW).unwrap();
        let mut png = Vec::new();
        image::RgbaImage::new(4, 3)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let image = store
            .insert(
                Capture {
                    formats: vec![("image/png".into(), png)],
                    copied_at: 2,
                    source: None,
                    password_marked: false,
                },
                &LIMITS,
                NOW,
            )
            .unwrap();
        let long = store
            .insert(capture("a long enough entry", 3), &LIMITS, NOW)
            .unwrap();
        // Newest, and longer than the excerpt, but it trims down to "x".
        let padded = format!(
            "{}x",
            " ".repeat(super::super::clipboard::EXCERPT_CHARS + 1)
        );
        let blank = store.insert(capture(&padded, 4), &LIMITS, NOW).unwrap();

        let settings = settings(3, Vec::new());
        let mut run = run(store, &settings);
        assert_eq!(run.resolve_one(&["1".into()]).unwrap().0.id, long);
        assert_eq!(run.resolve_one(&["2".into()]).unwrap().0.id, image);
        assert_eq!(
            run.resolve_one(std::slice::from_ref(&short)).unwrap().1,
            None,
            "shorter than min_chars"
        );
        assert_eq!(
            run.resolve_one(std::slice::from_ref(&blank)).unwrap().1,
            None,
            "the trimmed length decides, not the stored one"
        );
        assert_eq!(run.snapshot().unwrap().1.len(), 2);
        drop(run);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_a_listing_drops_rows_recorded_after_the_snapshot() {
        let page = Page {
            tab: "raffi".into(),
            total: 2,
            head: String::new(),
            next: 2,
            stable: true,
            items: vec![
                item("aaaaaaaaaa", ItemKind::Text, "known"),
                item("bbbbbbbbbb", ItemKind::Text, "recorded later"),
            ],
        };
        let eligible: HashSet<String> = ["aaaaaaaaaa".to_string()].into_iter().collect();
        let index: HashMap<String, usize> = [("aaaaaaaaaa".to_string(), 1)].into_iter().collect();
        let rows = selected(&page, &eligible, &index);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0.id, "aaaaaaaaaa");
        assert_eq!(rows[0].1, Some(1));
    }

    #[test]
    fn test_references_resolve_against_one_snapshot() {
        let (mut store, dir) = temp_store("refs");
        let mut ids = Vec::new();
        for (position, text) in ["alpha entry", "beta entry", "gamma entry"]
            .into_iter()
            .enumerate()
        {
            ids.push(
                store
                    .insert(capture(text, position as i64 + 1), &LIMITS, NOW)
                    .unwrap(),
            );
        }
        let settings = settings(3, Vec::new());
        let mut run = run(store, &settings);

        assert_eq!(run.resolve_one(&["1".into()]).unwrap().0.id, ids[2]);
        assert_eq!(run.resolve_one(&["3".into()]).unwrap().0.id, ids[0]);
        assert_eq!(
            run.resolve_one(&[ids[1].clone()]).unwrap().0.id,
            ids[1],
            "a whole id is a prefix reference"
        );
        assert_eq!(
            run.resolve_one(&[ids[1][..8].to_string()]).unwrap().0.id,
            ids[1]
        );
        let out_of_range = run.resolve_one(&["9".into()]).unwrap_err().message;
        assert!(out_of_range.contains("at index 9"), "{out_of_range}");
        assert!(run
            .resolve_one(&["ffffffffffffffff".into()])
            .unwrap_err()
            .message
            .contains("no clipboard entry matches"));

        // Repeats collapse, and nothing is removed while the others resolve.
        let (chosen, _) = run
            .resolve_all(
                &[ids[2].clone(), ids[2].clone(), ids[2][..8].to_string()],
                false,
            )
            .unwrap();
        assert_eq!(chosen.len(), 1, "the same entry named twice");
        assert_eq!(run.eligible().unwrap().items.len(), 3);
        let (chosen, _) = run
            .resolve_all(&[ids[0].clone(), ids[1].clone()], false)
            .unwrap();
        assert_eq!(chosen.len(), 2);
        assert!(run
            .resolve_all(&["1".to_string()], false)
            .unwrap_err()
            .message
            .contains("changes as the history moves"));
        drop(run);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_short_entries_are_referenceable_but_not_indexed() {
        let (mut store, dir) = temp_store("short");
        let short = store.insert(capture("ok", 1), &LIMITS, NOW).unwrap();
        let long = store
            .insert(capture("a longer entry", 2), &LIMITS, NOW)
            .unwrap();
        let settings = settings(3, Vec::new());
        let mut run = run(store, &settings);

        assert_eq!(
            run.resolve_one(std::slice::from_ref(&short)).unwrap().0.id,
            short
        );
        assert_eq!(
            run.resolve_one(&[short]).unwrap().1,
            None,
            "no index for an entry the default listing hides"
        );
        assert_eq!(run.resolve_one(&[long]).unwrap().1, Some(1));
        drop(run);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_timestamps_are_rfc_3339_and_keep_their_instant() {
        let formatted = unix_rfc3339(NOW).unwrap();
        let parsed = chrono::DateTime::parse_from_rfc3339(&formatted).unwrap();
        assert_eq!(parsed.timestamp(), NOW, "the instant survives the format");
        assert_eq!(
            formatted.len(),
            "2026-11-12T12:07:27+01:00".len(),
            "{formatted}"
        );
        assert_eq!(&formatted[19..20], "+", "{formatted}");

        // A fixed offset stands in for a timezone whose clock passed the same
        // wall-clock time twice: what the output carries is the instant, with
        // the offset it really had, not the first time that wall clock read.
        let after = chrono::FixedOffset::east_opt(60 * 60).unwrap();
        let before = chrono::FixedOffset::east_opt(2 * 60 * 60).unwrap();
        let wall_clock =
            NaiveDateTime::parse_from_str("2026-10-27 02:05:00", "%Y-%m-%d %H:%M:%S").unwrap();
        let first = after.from_local_datetime(&wall_clock).unwrap().timestamp();
        let earlier = before.from_local_datetime(&wall_clock).unwrap().timestamp();
        assert_eq!(
            earlier,
            first - 3600,
            "the two occurrences are an hour apart"
        );
        let formatted = rfc3339_at(first, &after).unwrap();
        assert_eq!(formatted, "2026-10-27T02:05:00+01:00");
        let parsed = chrono::DateTime::parse_from_rfc3339(&formatted).unwrap();
        assert_eq!(parsed.timestamp(), first, "not the first occurrence");
        let formatted = rfc3339_at(earlier, &before).unwrap();
        assert_eq!(
            formatted, "2026-10-27T02:05:00+02:00",
            "the same wall clock"
        );
        assert_eq!(
            chrono::DateTime::parse_from_rfc3339(&formatted)
                .unwrap()
                .timestamp(),
            earlier
        );
    }

    #[test]
    fn test_listing_keeps_the_order_of_the_history_and_carries_the_index() {
        let (mut store, dir) = temp_store("list");
        for (position, text) in ["first entry", "second entry"].into_iter().enumerate() {
            store
                .insert(capture(text, position as i64 + 1), &LIMITS, NOW)
                .unwrap();
        }
        let settings = settings(3, Vec::new());
        let mut run = run(store, &settings);
        let output = list(
            &mut run,
            &ClipboardListArgs {
                query: vec!["entry".into()],
                ..Default::default()
            },
        )
        .unwrap();
        let lines = String::from_utf8(output.stdout).unwrap();
        let fields: Vec<Vec<&str>> = lines
            .lines()
            .map(|line| line.split('\t').collect())
            .collect();
        assert_eq!(fields.len(), 2);
        assert_eq!(fields[0][1], "text");
        assert_eq!(fields[0][3], "second entry");
        assert_eq!(fields[0][0].len(), 32, "the whole id, not a prefix");
        assert_eq!(fields[0][2], unix_rfc3339(2).unwrap());

        let limited = list(
            &mut run,
            &ClipboardListArgs {
                limit: Some(1),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(limited.stdout).unwrap().lines().count(),
            1
        );
        drop(run);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_a_listing_can_skip_rows_and_drop_old_ones() {
        let (mut store, dir) = temp_store("paging");
        for (text, at) in [
            ("old entry", NOW - 7200),
            ("middle entry", NOW - 2400),
            ("new entry", NOW - 60),
        ] {
            store.insert(capture(text, at), &LIMITS, NOW).unwrap();
        }
        let settings = settings(3, Vec::new());
        let mut run = run(store, &settings);
        let previews = |output: Output| -> Vec<String> {
            String::from_utf8(output.stdout)
                .unwrap()
                .lines()
                .map(|line| line.split('\t').nth(3).unwrap().to_string())
                .collect()
        };
        let listing = |run: &mut Run, args: ClipboardListArgs| previews(list(run, &args).unwrap());

        assert_eq!(
            listing(&mut run, ClipboardListArgs::default()),
            ["new entry", "middle entry", "old entry"]
        );
        assert_eq!(
            listing(
                &mut run,
                ClipboardListArgs {
                    offset: Some(1),
                    ..Default::default()
                }
            ),
            ["middle entry", "old entry"]
        );
        assert_eq!(
            listing(
                &mut run,
                ClipboardListArgs {
                    offset: Some(1),
                    limit: Some(1),
                    ..Default::default()
                }
            ),
            ["middle entry"],
            "--offset applies before --limit"
        );
        assert_eq!(
            listing(
                &mut run,
                ClipboardListArgs {
                    since: Some("1h".into()),
                    ..Default::default()
                }
            ),
            ["new entry", "middle entry"]
        );
        assert_eq!(
            listing(
                &mut run,
                ClipboardListArgs {
                    since: Some("30m".into()),
                    ..Default::default()
                }
            ),
            ["new entry"]
        );
        assert_eq!(
            listing(
                &mut run,
                ClipboardListArgs {
                    since: Some("3d".into()),
                    ..Default::default()
                }
            )
            .len(),
            3
        );
        let since = unix_rfc3339(NOW - 3600).unwrap();
        assert_eq!(
            listing(
                &mut run,
                ClipboardListArgs {
                    since: Some(since),
                    ..Default::default()
                }
            ),
            ["new entry", "middle entry"]
        );

        // A skipped row keeps the position it has in the whole snapshot.
        let json = list(
            &mut run,
            &ClipboardListArgs {
                offset: Some(1),
                json: true,
                ..Default::default()
            },
        )
        .unwrap();
        let rows: Vec<serde_json::Value> = String::from_utf8(json.stdout)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["index"], 2);
        assert_eq!(rows[1]["index"], 3);

        let invalid = list(
            &mut run,
            &ClipboardListArgs {
                since: Some("soon\u{1b}".into()),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(invalid.message.contains("is not a duration"), "{invalid}");
        assert!(!invalid.message.contains('\u{1b}'), "{invalid}");
        assert_eq!(invalid.code, Code::InvalidArgument);
        assert_eq!(invalid.exit_code(), 2);
        drop(run);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_since_reads_durations_dates_and_timestamps() {
        assert_eq!(duration_secs("30s"), Some(30));
        assert_eq!(duration_secs("15m"), Some(15 * 60));
        assert_eq!(duration_secs("2h"), Some(2 * 60 * 60));
        assert_eq!(duration_secs("3d"), Some(3 * 24 * 60 * 60));
        for not_a_duration in ["", "s", "h", "-5m", "1.5h", "1w", "2 h", "soon"] {
            assert_eq!(duration_secs(not_a_duration), None, "{not_a_duration}");
        }
        assert_eq!(parse_since("1h", NOW).unwrap(), NOW - 3600);
        // A date is local midnight, whatever the timezone of the machine.
        let midnight =
            NaiveDateTime::parse_from_str("2026-11-12 00:00:00", "%Y-%m-%d %H:%M:%S").unwrap();
        assert_eq!(
            parse_since("2026-11-12", NOW).unwrap(),
            local_midnight_unix(midnight)
        );
        // A timestamp keeps its instant, whatever offset it was written in,
        // and a wall-clock time the clock passed twice keeps the offset it
        // was written with.
        let stamp = "2026-10-27T02:05:00+01:00";
        let secs = chrono::DateTime::parse_from_rfc3339(stamp)
            .unwrap()
            .timestamp();
        assert_eq!(parse_since(stamp, NOW).unwrap(), secs);
        let repeated = "2026-10-27T02:05:00+02:00";
        let other = chrono::DateTime::parse_from_rfc3339(repeated)
            .unwrap()
            .timestamp();
        assert_ne!(secs, other, "the two occurrences are an hour apart");
        assert_eq!(parse_since(repeated, NOW).unwrap(), other);
        let utc = "2026-11-12T12:07:27Z";
        let secs = chrono::DateTime::parse_from_rfc3339(utc)
            .unwrap()
            .timestamp();
        assert_eq!(parse_since(utc, NOW).unwrap(), secs);
        // A value that is not a duration is a usage error, never a panic on a
        // character boundary.
        for invalid in ["\u{e9}", "1\u{e9}", "30m\u{0}", "s", ""] {
            let error = parse_since(invalid, NOW).unwrap_err();
            assert_eq!(error.code, Code::InvalidArgument, "{invalid:?}");
            assert_eq!(error.exit_code(), 2, "{invalid:?}");
        }
        let invalid = parse_since("2026-13-45", NOW).unwrap_err();
        assert!(invalid.message.contains("2026-13-45"), "{invalid}");
        assert_eq!(invalid.exit_code(), 2);
    }

    #[test]
    fn test_since_drops_entries_without_a_copy_time() {
        let mut timed = item("aaaaaaaaaa", ItemKind::Text, "timed");
        timed.copied = Some(NOW);
        let mut older = item("bbbbbbbbbb", ItemKind::Text, "older");
        older.copied = Some(NOW - 24 * 60 * 60);
        let untimed = item("cccccccccc", ItemKind::Text, "untimed");
        let items = vec![(&timed, Some(1)), (&older, Some(2)), (&untimed, Some(3))];
        let kept = window(items, Some(NOW - 12 * 60 * 60), 0, None);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].0.id, "aaaaaaaaaa");
        assert_eq!(kept[0].1, Some(1));
        // Without --since, an entry with no copy time stays.
        let all = window(
            vec![(&timed, Some(1)), (&untimed, Some(3))],
            None,
            0,
            Some(2),
        );
        assert_eq!(all.len(), 2);
        // An offset past the end leaves nothing.
        assert!(window(vec![(&timed, Some(1))], None, 5, None).is_empty());
    }

    #[test]
    fn test_json_lines_carry_every_field() {
        let mut one = item("3fa9c1d2e0", ItemKind::Text, "git push");
        one.formats = vec!["text/plain".into()];
        one.copied = Some(NOW);
        one.expires_at = Some(NOW + 45);
        let rows = [
            row(&one, Some(1), None, None),
            row(
                &item("a81be0c44f", ItemKind::Image, ""),
                None,
                Some(&ImageMeta {
                    mime: "image/png".into(),
                    width: 1920,
                    height: 1080,
                    bytes: 1_200_000,
                }),
                None,
            ),
        ];
        let lines = json_lines(&rows).unwrap();
        let parsed: Vec<serde_json::Value> = lines
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(parsed[0]["index"], 1);
        assert_eq!(parsed[0]["id"], "3fa9c1d2e0");
        assert_eq!(parsed[0]["kind"], "text");
        assert_eq!(parsed[0]["tier"], 0);
        assert_eq!(parsed[0]["formats"][0], "text/plain");
        assert_eq!(parsed[0]["text_len"], 8);
        assert_eq!(parsed[0]["expires_at"], NOW + 45);
        assert_eq!(parsed[0]["expires_at_iso"], unix_rfc3339(NOW + 45).unwrap());
        assert_eq!(
            parsed[0]["copied"],
            unix_rfc3339(one.copied.unwrap()).unwrap()
        );
        assert!(parsed[1]["index"].is_null());
        assert_eq!(parsed[1]["kind"], "image");
        assert!(parsed[1]["copied"].is_null());
        assert!(parsed[1]["expires_at_iso"].is_null());
        assert_eq!(parsed[1]["width"], 1920);
        assert_eq!(parsed[1]["height"], 1080);
        assert_eq!(parsed[1]["preview"], "PNG image 1920×1080");
        // Anything that is not an image carries null dimensions.
        assert!(parsed[0]["width"].is_null());
        assert!(parsed[0]["height"].is_null());
    }

    #[test]
    fn test_json_rows_carry_the_desktop_name_of_their_source() {
        let dir = temp_applications("row-name");
        let apps = dir.join("applications");
        std::fs::write(
            apps.join("pi.desktop"),
            "[Desktop Entry]\nType=Application\nName=Pi Agent\n",
        )
        .unwrap();
        let mut sources = SourceNames {
            entries: Some(crate::DesktopEntries::from_dirs(&[apps])),
            names: HashMap::new(),
        };
        let mut named = item("3fa9c1d2e0", ItemKind::Text, "git push");
        named.source = Some("pi".into());
        let mut labelled = item("a81be0c44f", ItemKind::Text, "hello");
        labelled.source = Some("free label".into());
        let rows = [
            row(&named, Some(1), None, Some(&mut sources)),
            row(&labelled, Some(2), None, Some(&mut sources)),
            row(
                &item("b1b2c3d4e5", ItemKind::Text, "plain"),
                None,
                None,
                None,
            ),
        ];
        let lines = json_lines(&rows).unwrap();
        let parsed: Vec<serde_json::Value> = lines
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(parsed[0]["source"], "pi");
        assert_eq!(parsed[0]["source_name"], "Pi Agent");
        assert_eq!(parsed[1]["source"], "free label");
        assert!(
            parsed[1]["source_name"].is_null(),
            "a label no entry names has no name"
        );
        assert!(parsed[2]["source"].is_null());
        assert!(parsed[2]["source_name"].is_null());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_json_rows_carry_the_sanitized_name_of_their_source() {
        let dir = temp_applications("row-name-sanitized");
        let apps = dir.join("applications");
        std::fs::write(
            apps.join("evil.desktop"),
            "[Desktop Entry]\nType=Application\nName=Evil\u{1b}[31m\u{202e}txt\n",
        )
        .unwrap();
        let mut sources = SourceNames {
            entries: Some(crate::DesktopEntries::from_dirs(&[apps])),
            names: HashMap::new(),
        };
        let mut named = item("3fa9c1d2e0", ItemKind::Text, "git push");
        named.source = Some("evil".into());
        let rows = [row(&named, Some(1), None, Some(&mut sources))];
        let lines = json_lines(&rows).unwrap();
        let parsed: Vec<serde_json::Value> = lines
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(parsed[0]["source_name"], "Evil [31mtxt");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_json_rows_carry_the_search_tier() {
        let (mut store, dir) = temp_store("tier");
        for (position, text) in ["alpha beta", "x alpha beta", "beta x alpha"]
            .into_iter()
            .enumerate()
        {
            store
                .insert(capture(text, position as i64 + 1), &LIMITS, NOW)
                .unwrap();
        }
        let settings = settings(3, Vec::new());
        let mut run = run(store, &settings);
        let output = list(
            &mut run,
            &ClipboardListArgs {
                query: vec!["alpha".into(), "beta".into()],
                json: true,
                ..Default::default()
            },
        )
        .unwrap();
        let lines = String::from_utf8(output.stdout).unwrap();
        let parsed: Vec<serde_json::Value> = lines
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let tier_of = |preview: &str| {
            parsed
                .iter()
                .find(|row| row["preview"] == preview)
                .map(|row| row["tier"].as_u64().unwrap())
                .unwrap()
        };
        assert_eq!(tier_of("alpha beta"), 0, "starts with the phrase");
        assert_eq!(tier_of("x alpha beta"), 1, "contains the phrase");
        assert_eq!(tier_of("beta x alpha"), 2, "contains every word");
        drop(run);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_tab_separated_lines_cannot_be_broken_by_an_entry() {
        let nasty = "one\ttwo\r\nthree\u{1b}]52;c;ZXZpbA==\u{7}four";
        let mut entry = item("a1b2c3d4e5", ItemKind::Text, nasty);
        entry.text_len = nasty.len();
        let lines = tsv(&[row(&entry, Some(1), None, None)]);
        assert_eq!(lines.lines().count(), 1);
        let fields: Vec<&str> = lines.trim_end().split('\t').collect();
        assert_eq!(fields.len(), 4, "{lines:?}");
        assert!(!fields[3].contains('\u{1b}'), "{lines:?}");
        assert!(!fields[3].contains('\u{7}'), "{lines:?}");
        assert!(fields[3].contains("one two three"));
    }

    #[test]
    fn test_sanitizing_a_control_sequence() {
        let escape = "\u{1b}]52;c;ZXZpbA==\u{7}";
        assert_eq!(sanitize(escape, Sanitize::Spaces), " ]52;c;ZXZpbA== ");
        assert_eq!(sanitize(escape, Sanitize::Carets), "^[]52;c;ZXZpbA==^G");
        assert_eq!(sanitize("a\rb\tc\nd", Sanitize::Carets), "a^Mb^Ic^Jd");
        assert_eq!(sanitize("a\rb\tc\nd", Sanitize::Spaces), "a b c d");
        assert_eq!(sanitize("del\u{7f}", Sanitize::Carets), "del^?");
        assert_eq!(sanitize("x\u{85}y", Sanitize::Spaces), "x y");
        // Bidi overrides would reorder whatever follows them.
        assert_eq!(sanitize("a\u{202e}b\u{2066}c", Sanitize::Carets), "abc");
        assert_eq!(sanitize("héllo", Sanitize::Carets), "héllo");
    }

    #[test]
    fn test_the_table_fits_the_terminal_and_marks_temporary_entries() {
        let mut temporary = item("a81be0c44f", ItemKind::Text, "hunter2-temporary-token");
        temporary.copied = Some(NOW - 60);
        temporary.expires_at = Some(NOW + 45);
        let rows = [
            TableRow::of(&temporary, Some(2), Clock::at(NOW), None),
            TableRow::of(
                &item("77aa01fe3c", ItemKind::Image, ""),
                None,
                Clock::at(NOW),
                None,
            ),
        ];
        assert_eq!(
            rows[0].when,
            format!(
                "{} ⏱ 45s",
                short_copy_time(
                    local_time(temporary.copied.unwrap()).unwrap(),
                    Clock::at(NOW).local
                )
            )
        );
        assert_eq!(rows[1].index, "-");
        assert_eq!(rows[1].id, "77aa01fe3c");
        assert_eq!(rows[1].when, "");

        let wide = table(&rows, 200, false);
        assert!(wide.starts_with("#  ID"), "{wide}");
        assert!(wide.contains("a81be0c44f"), "{wide}");
        assert!(wide.contains("hunter2-temporary-token"), "{wide}");
        let narrow = table(&rows, 50, false);
        for line in narrow.lines() {
            assert!(line.width() <= 50, "{line:?}");
        }
        assert!(narrow.contains('…'));
        // Colour only ever wraps whole cells.
        let coloured = table(&rows, 200, true);
        assert!(coloured.contains("\u{1b}[2m"), "{coloured}");
        assert!(!table(&rows[..0], 80, false).contains('#'));
    }

    #[test]
    fn test_the_table_measures_display_columns() {
        let rows = [TableRow::of(
            &item("77aa01fe3c", ItemKind::Text, &"界".repeat(60)),
            Some(1),
            Clock::at(NOW),
            None,
        )];
        let narrow = table(&rows, 50, false);
        for line in narrow.lines() {
            assert!(line.width() <= 50, "{line:?}");
        }
        assert!(narrow.contains('…'));
        // A combining mark takes no column of its own.
        assert_eq!(fit("e\u{301}e\u{301}", 2), "e\u{301}e\u{301}");
        assert_eq!(fit("e\u{301}e\u{301}e\u{301}", 2), "e\u{301}…");
        assert_eq!(fit("界界界", 5), "界界…");
        assert_eq!(fit("界界", 4), "界界");
    }

    #[test]
    fn test_hostile_mime_names_cannot_drive_the_terminal() {
        let escape = "\u{1b}]52;c;ZXZpbA==\u{7}";
        let formats = [
            (format!("text/plain{escape}"), 12),
            ("image/png".to_string(), 4),
        ];
        let human = formats_output(&formats, false).unwrap();
        let human = String::from_utf8(human.stdout).unwrap();
        assert!(!human.contains('\u{1b}'), "{human:?}");
        assert!(!human.contains('\u{7}'), "{human:?}");
        assert!(human.contains("image/png"), "{human:?}");
        // The JSON listing keeps the exact stored strings.
        let json = formats_output(&formats, true).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
        assert_eq!(value["formats"][0]["mime"], formats[0].0);
        assert_eq!(value["formats"][1]["mime"], "image/png");
    }

    #[test]
    fn test_an_image_refusal_sanitizes_its_type() {
        let (mut store, dir) = temp_store("hostile-image");
        let id = store
            .insert(
                Capture {
                    formats: vec![("image/pn\u{1b}]52;c;ZXZpbA==\u{7}g".into(), b"png".to_vec())],
                    copied_at: 1,
                    source: None,
                    password_marked: false,
                },
                &LIMITS,
                NOW,
            )
            .unwrap();
        let settings = settings(3, Vec::new());
        let mut run = run(store, &settings);
        run.io.stdout_tty = true;
        let args = ClipboardShowArgs {
            reference: vec![id.clone()],
            ..Default::default()
        };
        let refused = show(&mut run, &args, std::slice::from_ref(&id))
            .unwrap_err()
            .message;
        assert!(refused.contains("redirect its output"), "{refused:?}");
        assert!(!refused.contains('\u{1b}'), "{refused:?}");
        assert!(!refused.contains('\u{7}'), "{refused:?}");
        drop(run);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_show_writes_the_stored_bytes_exactly() {
        let (mut store, dir) = temp_store("show-text");
        let text = "  line one\r\nline two\n";
        let id = store.insert(capture(text, 1), &LIMITS, NOW).unwrap();
        let settings = settings(3, Vec::new());
        let mut run = run(store, &settings);

        let args = ClipboardShowArgs {
            reference: vec![id.clone()],
            ..Default::default()
        };
        let output = show(&mut run, &args, std::slice::from_ref(&id)).unwrap();
        assert_eq!(
            output.stdout,
            text.as_bytes(),
            "no added newline, no cleanup"
        );

        let mime = ClipboardShowArgs {
            reference: vec![id.clone()],
            mime: Some("text/plain".into()),
            ..Default::default()
        };
        assert_eq!(
            show(&mut run, &mime, std::slice::from_ref(&id))
                .unwrap()
                .stdout,
            text.as_bytes()
        );
        let missing = ClipboardShowArgs {
            reference: vec![id.clone()],
            mime: Some("text/html".into()),
            ..Default::default()
        };
        let error = show(&mut run, &missing, std::slice::from_ref(&id))
            .unwrap_err()
            .message;
        assert!(error.contains("holds no text/html format"), "{error}");
        assert!(error.contains("text/plain"), "{error}");

        let formats = ClipboardShowArgs {
            reference: vec![id.clone()],
            formats: true,
            ..Default::default()
        };
        let listed = show(&mut run, &formats, std::slice::from_ref(&id)).unwrap();
        assert_eq!(
            String::from_utf8(listed.stdout).unwrap(),
            format!("text/plain  {} B\n", text.len())
        );

        let json = ClipboardShowArgs {
            reference: vec![id],
            json: true,
            ..Default::default()
        };
        let metadata = show(&mut run, &json, &args.reference).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&metadata.stdout).unwrap();
        assert_eq!(value["kind"], "text");
        assert_eq!(value["text"], text);
        drop(run);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_max_bytes_caps_the_exported_content() {
        let (mut store, dir) = temp_store("show-max-bytes");
        let text = "héllo wörld";
        let id = store.insert(capture(text, 1), &LIMITS, NOW).unwrap();
        let mut png = Vec::new();
        image::RgbaImage::new(4, 3)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let image = store
            .insert(
                Capture {
                    formats: vec![("image/png".into(), png.clone())],
                    copied_at: 2,
                    source: None,
                    password_marked: false,
                },
                &LIMITS,
                NOW,
            )
            .unwrap();
        let settings = settings(3, Vec::new());
        let mut run = run(store, &settings);

        let capped = |run: &mut Run, id: &str, max: usize| {
            let args = ClipboardShowArgs {
                reference: vec![id.to_string()],
                max_bytes: Some(max),
                ..Default::default()
            };
            show(run, &args, &[id.to_string()]).unwrap()
        };

        let one = capped(&mut run, &id, 1);
        assert_eq!(one.stdout, b"h");
        assert_eq!(
            one.notices,
            [format!(
                "raffi clipboard: output truncated to 1 of {} bytes",
                text.len()
            )]
        );
        // Two bytes would split `é`, so the cut stops after the `h`.
        let two = capped(&mut run, &id, 2);
        assert_eq!(two.stdout, b"h");
        assert_eq!(
            two.notices,
            [format!(
                "raffi clipboard: output truncated to 1 of {} bytes",
                text.len()
            )]
        );
        // Exactly the length of the entry writes all of it, and says nothing.
        let whole = capped(&mut run, &id, text.len());
        assert_eq!(whole.stdout, text.as_bytes());
        assert_eq!(whole.notices, Vec::<String>::new());

        // A stored format is cut wherever the byte falls, text or not.
        let mime = ClipboardShowArgs {
            reference: vec![id.clone()],
            mime: Some("text/plain".into()),
            max_bytes: Some(2),
            ..Default::default()
        };
        let raw = show(&mut run, &mime, std::slice::from_ref(&id)).unwrap();
        assert_eq!(raw.stdout, text.as_bytes()[..2].to_vec());
        assert!(std::str::from_utf8(&raw.stdout).is_err());
        assert_eq!(raw.notices.len(), 1);

        let image_args = ClipboardShowArgs {
            reference: vec![image.clone()],
            max_bytes: Some(8),
            ..Default::default()
        };
        let cut_image = show(&mut run, &image_args, std::slice::from_ref(&image)).unwrap();
        assert_eq!(cut_image.stdout, png[..8].to_vec());
        assert_eq!(
            cut_image.notices,
            [format!(
                "raffi clipboard: output truncated to 8 of {} bytes",
                png.len()
            )]
        );

        // The JSON metadata is cut too, and keeps the length of the entry.
        let json = ClipboardShowArgs {
            reference: vec![id.clone()],
            json: true,
            ..Default::default()
        };
        let full: serde_json::Value = serde_json::from_slice(
            &show(&mut run, &json, std::slice::from_ref(&id))
                .unwrap()
                .stdout,
        )
        .unwrap();
        assert_eq!(full["truncated"], false);
        assert_eq!(full["text"], text);
        let cut_json = ClipboardShowArgs {
            max_bytes: Some(3),
            ..json
        };
        let value: serde_json::Value = serde_json::from_slice(
            &show(&mut run, &cut_json, std::slice::from_ref(&id))
                .unwrap()
                .stdout,
        )
        .unwrap();
        assert_eq!(value["truncated"], true);
        assert_eq!(value["text"], "hé");
        assert_eq!(value["text_len"], full["text_len"]);
        drop(run);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_show_refuses_to_write_an_image_to_a_terminal() {
        let (mut store, dir) = temp_store("show-image");
        let mut png = Vec::new();
        image::RgbaImage::new(4, 3)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let id = store
            .insert(
                Capture {
                    formats: vec![("image/png".into(), png.clone())],
                    copied_at: 1,
                    source: None,
                    password_marked: false,
                },
                &LIMITS,
                NOW,
            )
            .unwrap();
        let settings = settings(3, Vec::new());
        let mut run = run(store, &settings);
        let args = ClipboardShowArgs {
            reference: vec![id.clone()],
            ..Default::default()
        };
        assert_eq!(
            show(&mut run, &args, std::slice::from_ref(&id))
                .unwrap()
                .stdout,
            png
        );

        run.io.stdout_tty = true;
        let refused = show(&mut run, &args, std::slice::from_ref(&id))
            .unwrap_err()
            .message;
        assert!(refused.contains("redirect its output"), "{refused}");
        assert!(refused.contains("PNG"), "{refused}");
        drop(run);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_show_json_reports_an_image_by_its_size() {
        let (mut store, dir) = temp_store("show-image-json");
        let mut png = Vec::new();
        image::RgbaImage::new(6, 2)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let id = store
            .insert(
                Capture {
                    formats: vec![("image/png".into(), png)],
                    copied_at: 1,
                    source: None,
                    password_marked: false,
                },
                &LIMITS,
                NOW,
            )
            .unwrap();
        let settings = settings(3, Vec::new());
        let mut run = run(store, &settings);
        let value: serde_json::Value = serde_json::from_slice(
            &show(
                &mut run,
                &ClipboardShowArgs {
                    reference: vec![id.clone()],
                    json: true,
                    ..Default::default()
                },
                std::slice::from_ref(&id),
            )
            .unwrap()
            .stdout,
        )
        .unwrap();
        assert_eq!(value["kind"], "image");
        assert_eq!(value["width"], 6, "the metadata the listing reports");
        assert_eq!(value["height"], 2);
        assert_eq!(value["preview"], "PNG image 6×2");
        // An image has no text to show, so the field is there and null.
        assert!(value["text"].is_null(), "{value}");
        assert!(value.get("text").is_some());
        assert_eq!(value["truncated"], false);
        drop(run);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_a_file_list_is_shown_as_paths() {
        let (mut store, dir) = temp_store("show-files");
        let list = "# copied files\r\nfile:///tmp/a%20b.txt\r\nhttps://example.com/x\r\n";
        let id = store
            .insert(
                Capture {
                    formats: vec![("text/uri-list".into(), list.as_bytes().to_vec())],
                    copied_at: 1,
                    source: None,
                    password_marked: false,
                },
                &LIMITS,
                NOW,
            )
            .unwrap();
        let settings = settings(3, Vec::new());
        let mut run = run(store, &settings);
        let output = show(
            &mut run,
            &ClipboardShowArgs {
                reference: vec![id.clone()],
                ..Default::default()
            },
            &[id],
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            "/tmp/a b.txt\nhttps://example.com/x\n"
        );
        drop(run);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_a_deletion_says_what_it_did() {
        let quiet = deleted(2, Io::pipe(), true);
        assert_eq!(quiet, Output::default());
        assert_eq!(deleted(1, Io::pipe(), true).notices, Vec::<String>::new());
        let mut io = Io::pipe();
        io.stderr_tty = true;
        assert_eq!(deleted(1, io, true).notices, ["Deleted 1 entry"]);
        assert_eq!(deleted(3, io, true).notices, ["Deleted 3 entries"]);
        // A log that still holds old pages is always reported.
        assert_eq!(
            deleted(1, Io::pipe(), false).notices,
            ["deleted; the database log still holds old pages until the next checkpoint"]
        );
    }

    #[test]
    fn test_the_status_block_is_aligned() {
        let status = Status {
            backend: "raffi",
            configured: "auto",
            recorder: true,
            protocol: Some(super::super::cache::PROTOCOL),
            database: Some("/home/me/.local/share/raffi/clipboard.db".into()),
            database_bytes: Some(13_000_000),
            entries: 842,
            temporary: 3,
            limits: LimitsReport::from(LIMITS),
        };
        let text = status_text(&status);
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[0].starts_with("Backend     raffi (auto)"), "{text}");
        assert!(
            lines[1].starts_with("Recorder    running (raffi --cache-daemon)"),
            "{text}"
        );
        assert!(
            lines[2].starts_with("Database    /home/me/.local/share/raffi/clipboard.db ("),
            "{text}"
        );
        assert!(
            lines[3].starts_with("Entries     842 (3 temporary)"),
            "{text}"
        );
        assert!(lines[4].starts_with("Limits      100 entries"), "{text}");
        assert_eq!(
            limits_line(&LimitsReport::from(LIMITS)),
            "100 entries, 1024 MB, images ≤ 1 MB"
        );
        let json = serde_json::to_value(&status).unwrap();
        assert_eq!(json["backend"], "raffi");
        assert_eq!(json["configured"], "auto");
        assert_eq!(json["protocol"], super::super::cache::PROTOCOL);
        assert_eq!(json["recorder"], true);
        assert_eq!(json["entries"], 842);
        assert_eq!(json["temporary"], 3);
        assert_eq!(json["max_items"], 100);

        // A backend that keeps its own storage has no path or size to show.
        let copyq = Status {
            backend: "copyq",
            configured: "copyq",
            recorder: false,
            protocol: None,
            database: None,
            database_bytes: None,
            entries: 0,
            temporary: 0,
            limits: LimitsReport::from(LIMITS),
        };
        let text = status_text(&copyq);
        assert!(
            text.contains("Database    not used by this backend"),
            "{text}"
        );
        assert!(!text.contains(" B)"), "{text}");
        let json = serde_json::to_value(&copyq).unwrap();
        assert_eq!(json["backend"], "copyq");
        assert_eq!(json["configured"], "copyq");
        assert_eq!(json["temporary"], 0);
        assert!(json["protocol"].is_null());
        assert!(json["database"].is_null());
        assert!(json["database_bytes"].is_null());
    }

    fn set_args(text: &[&str], mime: Option<&str>) -> ClipboardSetArgs {
        ClipboardSetArgs {
            mime: mime.map(str::to_string),
            text: text.iter().map(|word| word.to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn test_set_reads_its_arguments_or_its_input() {
        let mut input: &[u8] = b"ignored";
        assert_eq!(
            content(
                "text/plain",
                &set_args(&["one", "two"], None),
                Io::pipe(),
                &LIMITS,
                &mut input
            )
            .unwrap(),
            b"one two"
        );

        let mut input: &[u8] = b"from a pipe\nwith two lines";
        assert_eq!(
            content(
                "text/plain",
                &set_args(&[], None),
                Io::pipe(),
                &LIMITS,
                &mut input
            )
            .unwrap(),
            b"from a pipe\nwith two lines"
        );

        // A terminal is never read: it would wait for the reader to type.
        let mut io = Io::pipe();
        io.stdin_tty = true;
        let mut input: &[u8] = b"typed";
        let refused = content("text/plain", &set_args(&[], None), io, &LIMITS, &mut input)
            .unwrap_err()
            .message;
        assert!(refused.contains("no content"), "{refused}");

        // Reading stops one byte past the limit of the type.
        let small = Limits {
            max_image_size: 4,
            ..LIMITS
        };
        let mut input: &[u8] = b"0123456789";
        let binary = content(
            "image/png",
            &set_args(&[], None),
            Io::pipe(),
            &small,
            &mut input,
        )
        .unwrap();
        assert_eq!(binary, b"01234", "one byte over the limit is enough");
    }

    #[test]
    fn test_set_validates_its_content() {
        assert!(check_content("text/plain", b"hi", &LIMITS).is_ok());
        assert_eq!(
            check_content("text/plain", b"", &LIMITS)
                .unwrap_err()
                .message,
            "the content is empty"
        );
        let not_utf8 = check_content("text/plain", b"\xff\xfe", &LIMITS)
            .unwrap_err()
            .message;
        assert!(not_utf8.contains("not UTF-8"), "{not_utf8}");
        assert!(check_content("text/html", b"\xff", &LIMITS).is_err());
        // Binary content is fine under a type that is not text.
        assert!(check_content("image/png", b"\x89PNG", &LIMITS).is_ok());
        assert!(check_content("application/octet-stream", b"\x00\x01", &LIMITS).is_ok());

        let small = Limits {
            max_image_size: 3,
            ..LIMITS
        };
        let over = check_content("image/png", b"\x89PNG", &small)
            .unwrap_err()
            .message;
        assert!(over.contains("over the 3 B limit for image/png"), "{over}");
        let text_over = check_content(
            "text/plain",
            &vec![b'a'; clipboard_store::MAX_TEXT_BYTES + 1],
            &LIMITS,
        )
        .unwrap_err()
        .message;
        assert!(text_over.contains("limit for text/plain"), "{text_over}");
    }

    #[test]
    fn test_set_stores_plain_text_under_its_media_type() {
        for alias in [
            "text/plain",
            "text/plain;charset=utf-8",
            "TEXT",
            "UTF8_STRING",
            " string ",
        ] {
            assert_eq!(canonical_mime(alias).unwrap(), "text/plain", "{alias}");
            assert!(is_text(alias), "{alias}");
        }
        // Other text types keep their name, so the history files and formats
        // them as it does a copy.
        assert_eq!(canonical_mime("text/html").unwrap(), "text/html");
        assert_eq!(canonical_mime("text/uri-list").unwrap(), "text/uri-list");
        assert_eq!(canonical_mime("image/png").unwrap(), "image/png");
        assert_eq!(canonical_mime(" image/png ").unwrap(), "image/png");
        assert!(!is_text("image/png"));
        assert!(!is_text("application/json"));
        assert!(canonical_mime("  ")
            .unwrap_err()
            .message
            .contains("empty MIME type"));
    }

    #[test]
    fn test_set_prints_the_entry_it_recorded() {
        let recorded = set_output(Some("3fa9c1d2e0"), true, None, false).unwrap();
        assert_eq!(String::from_utf8(recorded.stdout).unwrap(), "3fa9c1d2e0\n");
        // Content the history kept out prints nothing at all.
        assert_eq!(
            set_output(None, false, None, false).unwrap(),
            Output::default()
        );

        let json = set_output(Some("3fa9c1d2e0"), true, Some(NOW + 60), true).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
        assert_eq!(value["id"], "3fa9c1d2e0");
        assert_eq!(value["recorded"], true);
        assert_eq!(value["expires_at"], NOW + 60);
        assert_eq!(value["expires_at_iso"], unix_rfc3339(NOW + 60).unwrap());

        let unrecorded = set_output(None, false, None, true).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&unrecorded.stdout).unwrap();
        assert!(value["id"].is_null());
        assert_eq!(value["recorded"], false);
        assert!(value["expires_at"].is_null());
        assert!(value["expires_at_iso"].is_null());
    }

    #[test]
    fn test_set_needs_the_clipboard_service() {
        let (store, dir) = temp_store("set-service");
        let settings = settings(3, Vec::new());
        let mut run = run(store, &settings);
        let error = set(&mut run, &set_args(&["hello"], None), &mut &b""[..])
            .unwrap_err()
            .message;
        assert!(error.contains("raffi --cache-daemon"), "{error}");
        drop(run);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_copyq_cannot_record_a_temporary_or_secret_set() {
        let settings = settings(3, Vec::new());
        let copyq_run = |args: ClipboardSetArgs| {
            let mut run = Run {
                clips: Clips::CopyQ(CopyQ::with_session("raffi-test")),
                settings: &settings,
                clock: Clock::at(NOW),
                io: Io::pipe(),
            };
            set(&mut run, &args, &mut &b""[..]).unwrap_err().message
        };
        let temporary = copyq_run(ClipboardSetArgs {
            temporary: true,
            ..set_args(&["hello"], None)
        });
        assert!(
            temporary.contains("not supported by the copyq backend"),
            "{temporary}"
        );
        let secret = copyq_run(ClipboardSetArgs {
            secret: true,
            ..set_args(&["hello"], None)
        });
        assert!(
            secret.contains("not supported by the copyq backend"),
            "{secret}"
        );
    }

    #[test]
    fn test_copyq_cannot_record_the_app_that_set_the_content() {
        let settings = settings(3, Vec::new());
        let mut run = Run {
            clips: Clips::CopyQ(CopyQ::with_session("raffi-test")),
            settings: &settings,
            clock: Clock::at(NOW),
            io: Io::pipe(),
        };
        let args = ClipboardSetArgs {
            source: Some("pi".into()),
            ..set_args(&["hello"], None)
        };
        let error = set(&mut run, &args, &mut &b""[..]).unwrap_err();
        assert_eq!(error.code, Code::Unsupported);
        assert!(
            error
                .message
                .contains("--source is not supported by the copyq backend"),
            "{error}"
        );
    }

    #[test]
    fn test_the_source_app_is_checked_before_it_is_recorded() {
        let dirs: &[std::path::PathBuf] = &[];
        assert_eq!(source_app(None, dirs).unwrap(), None);
        assert_eq!(source_app(Some("pi"), dirs).unwrap().as_deref(), Some("pi"));
        // The name is trimmed, and an agent's name has to fit on one line.
        assert_eq!(
            source_app(Some(" pi "), dirs).unwrap().as_deref(),
            Some("pi")
        );
        for bad in ["", "   ", "p\u{0}i", "pi\u{1b}[31m"] {
            let error = source_app(Some(bad), dirs).unwrap_err();
            assert_eq!(error.code, Code::InvalidArgument, "{bad:?}");
            assert_eq!(error.exit_code(), 2, "{bad:?}");
        }
        let long = "a".repeat(clipboard_store::MAX_SOURCE_BYTES + 1);
        assert_eq!(
            source_app(Some(&long), dirs).unwrap_err().code,
            Code::InvalidArgument
        );
        assert!(source_app(Some(&"a".repeat(clipboard_store::MAX_SOURCE_BYTES)), dirs).is_ok());
    }

    /// A directory holding one `applications` tree, as a test's XDG data dir.
    /// Every caller makes its own name so two tests never share a tree.
    fn temp_applications(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("raffi-clip-source-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("applications")).unwrap();
        dir
    }

    #[test]
    fn test_a_source_desktop_id_resolves_in_the_application_dirs() {
        let dir = temp_applications("id");
        let apps = dir.join("applications");
        std::fs::write(
            apps.join("pi.desktop"),
            "[Desktop Entry]\nType=Application\nName=Pi\nIcon=pi-agent\n",
        )
        .unwrap();
        std::fs::create_dir_all(apps.join("kde")).unwrap();
        std::fs::write(
            apps.join("kde").join("foo.desktop"),
            "[Desktop Entry]\nType=Application\nName=Foo\n",
        )
        .unwrap();
        let dirs = [apps.clone()];

        // The `.desktop` suffix is dropped, so the value matches an app_id.
        assert_eq!(
            source_app(Some("pi.desktop"), &dirs).unwrap().as_deref(),
            Some("pi")
        );
        // The ID keeps the case it was found under.
        assert_eq!(
            source_app(Some("PI.DESKTOP"), &dirs).unwrap().as_deref(),
            Some("pi")
        );
        // A subdirectory maps to a `-` in the ID.
        assert_eq!(
            source_app(Some("kde-foo.desktop"), &dirs)
                .unwrap()
                .as_deref(),
            Some("kde-foo")
        );
        // A window class is not an ID: `--source` names a file.
        std::fs::write(
            apps.join("other.desktop"),
            "[Desktop Entry]\nType=Application\nName=Other\nStartupWMClass=missing.desktop\n",
        )
        .unwrap();
        let missing = source_app(Some("missing.desktop"), &dirs).unwrap_err();
        assert_eq!(missing.code, Code::InvalidArgument);
        assert_eq!(missing.exit_code(), 2);
        assert_eq!(
            missing.message,
            format!("no desktop entry `missing.desktop` in {}", apps.display())
        );

        let missing = source_app(Some("nope.desktop"), &dirs).unwrap_err();
        assert_eq!(missing.code, Code::InvalidArgument);
        assert_eq!(missing.exit_code(), 2);
        assert_eq!(
            missing.message,
            format!("no desktop entry `nope.desktop` in {}", apps.display())
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_a_source_that_is_a_path_is_refused() {
        let dirs: &[std::path::PathBuf] = &[];
        for path in [
            "/home/me/.local/share/applications/pi.desktop",
            "apps/pi.desktop",
            "~/pi.desktop",
        ] {
            let error = source_app(Some(path), dirs).unwrap_err();
            assert_eq!(error.code, Code::InvalidArgument, "{path}");
            assert_eq!(error.exit_code(), 2, "{path}");
            assert!(
                error.message.contains("~/.local/share/applications"),
                "{path}: {}",
                error.message
            );
        }
    }

    #[test]
    fn test_a_copyq_entry_that_went_away_is_not_found() {
        let error = copyq_text_error("item is no longer in the clipboard history");
        assert_eq!(error.code, Code::NotFound);
    }

    #[test]
    fn test_a_day_starts_at_its_midnight_in_a_fixed_zone() {
        let zone = chrono::FixedOffset::east_opt(3600).unwrap();
        let midnight = chrono::NaiveDate::from_ymd_opt(2026, 3, 29)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap();
        assert_eq!(
            day_start_unix(&zone, midnight),
            midnight.and_utc().timestamp() - 3600
        );
    }

    #[test]
    fn test_errors_carry_a_stable_code() {
        let entries = [item("a1b2c3d4e5", ItemKind::Text, "newest")];
        let index = index_of(&entries);
        let code = |error: Error| error.code;
        assert_eq!(
            code(resolve(&Ref::Index(1), &entries, &index, false).unwrap_err()),
            Code::IndexNotAllowed
        );
        assert_eq!(
            code(resolve(&Ref::Index(2), &entries, &index, true).unwrap_err()),
            Code::NotFound
        );
        assert_eq!(
            code(resolve(&Ref::Prefix("deadbeef".into()), &entries, &index, true).unwrap_err()),
            Code::NotFound
        );
        assert_eq!(code(parse_ref("nope").unwrap_err()), Code::InvalidRef);
        assert_eq!(
            code(check_content("text/plain", b"", &LIMITS).unwrap_err()),
            Code::InvalidArgument
        );
        assert_eq!(
            code(check_content("text/plain", b"\xff", &LIMITS).unwrap_err()),
            Code::InvalidArgument
        );
        let small = Limits {
            max_image_size: 3,
            ..LIMITS
        };
        assert_eq!(
            code(check_content("image/png", b"\x89PNG", &small).unwrap_err()),
            Code::TooLarge
        );
        assert_eq!(
            code(canonical_mime(" ").unwrap_err()),
            Code::InvalidArgument
        );
        assert_eq!(
            code(copyq_text_error("the text is longer than 16 MiB")),
            Code::TooLarge
        );
        assert_eq!(code(copyq_text_error("copyq exited with 1")), Code::Io);
        // The two messages the backends raise keep their own code.
        assert_eq!(
            code(Error::from(super::super::clipboard::NO_BACKEND.to_string())),
            Code::NoBackend
        );
        assert_eq!(
            code(Error::from(
                super::super::cache::CLIPBOARD_NOT_RUNNING.to_string()
            )),
            Code::RecorderNotRunning
        );
        assert_eq!(
            code(Error::from("database is locked".to_string())),
            Code::Io
        );
        assert_eq!(
            code(Error::from(super::super::clipboard_store::GONE.to_string())),
            Code::NotFound,
            "an entry removed while it was read"
        );
        assert_eq!(
            code(Error::new(Code::AmbiguousRef, "two entries")),
            Code::AmbiguousRef
        );
        assert_eq!(
            [
                Code::NotFound,
                Code::AmbiguousRef,
                Code::InvalidRef,
                Code::IndexNotAllowed,
                Code::NoBackend,
                Code::RecorderNotRunning,
                Code::DaemonOutdated,
                Code::Unsupported,
                Code::TooLarge,
                Code::NoHistory,
                Code::Io,
                Code::InvalidArgument,
                Code::Other,
            ]
            .iter()
            .map(|code| code.name())
            .collect::<Vec<_>>(),
            [
                "not_found",
                "ambiguous_ref",
                "invalid_ref",
                "index_not_allowed",
                "no_backend",
                "recorder_not_running",
                "daemon_outdated",
                "unsupported",
                "too_large",
                "no_history",
                "io",
                "invalid_argument",
                "other",
            ]
        );
    }

    #[test]
    fn test_a_failed_service_call_keeps_the_code_it_gave() {
        use super::super::cache::{ErrorCode, SetFailure};
        let cases = [
            (SetFailure::DaemonOutdated(3), Code::DaemonOutdated),
            (SetFailure::NotRunning, Code::RecorderNotRunning),
            (
                SetFailure::Failed(Some(ErrorCode::TooLarge), "too large: x".into()),
                Code::TooLarge,
            ),
            (
                SetFailure::Failed(
                    Some(ErrorCode::InvalidArgument),
                    "a MIME type cannot hold a control character".into(),
                ),
                Code::InvalidArgument,
            ),
            (
                SetFailure::Failed(Some(ErrorCode::NotRecording), "not recording".into()),
                Code::RecorderNotRunning,
            ),
            (
                SetFailure::Failed(Some(ErrorCode::NotFound), "gone".into()),
                Code::NotFound,
            ),
            (
                SetFailure::Failed(Some(ErrorCode::Io), "database is locked".into()),
                Code::Io,
            ),
            (
                SetFailure::Failed(None, "the Raffi server closed the connection".into()),
                Code::Io,
            ),
        ];
        for (failure, expected) in cases {
            assert_eq!(Error::from(failure).code, expected);
        }
        // The message an older service gives still explains itself.
        let outdated = Error::from(SetFailure::DaemonOutdated(3));
        assert!(outdated.message.contains("restart `raffi --cache-daemon`"));
        assert!(
            outdated.message.contains("protocol 3"),
            "{}",
            outdated.message
        );
    }

    #[test]
    fn test_a_failed_write_is_a_failure_of_the_command() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::from(std::io::ErrorKind::StorageFull))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let error = write_stdout(&mut Broken, b"content").unwrap_err();
        assert_eq!(error.code, Code::Io);
        assert!(error.message.contains("cannot write to stdout"), "{error}");
        // Under --json it is still the one-line JSON error.
        let value: serde_json::Value = serde_json::from_str(&error.json_line()).unwrap();
        assert_eq!(value["code"], "io");

        let mut good = Vec::new();
        write_stdout(&mut good, b"content").unwrap();
        assert_eq!(good, b"content");
    }

    #[test]
    fn test_json_errors_are_one_line_and_usage_errors_exit_2() {
        let failed = Error::new(Code::NotFound, "no clipboard entry matches `deadbeef`");
        assert_eq!(
            failed.json_line(),
            r#"{"error":"no clipboard entry matches `deadbeef`","code":"not_found"}"#
        );
        assert_eq!(failed.exit_code(), 1);
        assert!(!failed.is_reported());
        assert!(failed.reported().is_reported());
        // A quote in the message cannot break the line.
        let quoted = Error::new(Code::Io, r#"a "b" \ c"#);
        let value: serde_json::Value = serde_json::from_str(&quoted.json_line()).unwrap();
        assert_eq!(value["error"], r#"a "b" \ c"#);
        assert_eq!(value["code"], "io");
        let usage = Error::usage("`soon` is not a duration");
        assert_eq!(usage.exit_code(), 2);
        assert_eq!(usage.code, Code::InvalidArgument);

        // Only the actions that print JSON get their errors as JSON.
        let json = ClipboardArgs {
            action: Some(ClipboardCommand::List(ClipboardListArgs {
                json: true,
                ..Default::default()
            })),
            ..Default::default()
        };
        assert!(json_requested(&json));
        let plain = ClipboardArgs {
            action: Some(ClipboardCommand::Show(ClipboardShowArgs::default())),
            ..Default::default()
        };
        assert!(!json_requested(&plain));
        assert!(!json_requested(&ClipboardArgs::default()));
        assert!(json_requested(&ClipboardArgs {
            action: Some(ClipboardCommand::Status(ClipboardStatusArgs {
                json: true,
                ..Default::default()
            })),
            ..Default::default()
        }));
    }

    #[test]
    fn test_kind_filters_are_explicit() {
        assert_eq!(kind_filter(None), Ok("1"));
        for (kind, predicate) in [
            (ClipboardKind::Text, "kind = 'text'"),
            (ClipboardKind::Html, "kind = 'html'"),
            (ClipboardKind::Image, "kind = 'image'"),
            (ClipboardKind::Files, "kind = 'files'"),
            (
                ClipboardKind::Url,
                "kind IN ('text', 'html') AND is_url = 1",
            ),
            (ClipboardKind::Other, "kind = 'other'"),
        ] {
            assert_eq!(kind_filter(Some(kind)), Ok(predicate));
        }
        assert_eq!("image".parse::<ClipboardKind>(), Ok(ClipboardKind::Image));
        assert!("images".parse::<ClipboardKind>().is_err());
        assert_eq!("other".parse::<ClipboardKind>().unwrap().name(), "other");
    }

    fn page(tab: &str, head: &str, total: usize, next: usize, stable: bool, ids: &[&str]) -> Page {
        Page {
            tab: tab.into(),
            total,
            head: head.into(),
            next,
            stable,
            items: ids
                .iter()
                .map(|id| item(id, ItemKind::Text, "an entry"))
                .collect(),
        }
    }

    #[test]
    fn test_copyq_listing_checks_every_page_and_retries_once() {
        // Two stable pages that agree join into one listing.
        let mut fetch = |_tab: &str, start: usize| {
            Ok(if start == 0 {
                page("raffi", "head", 3, 2, true, &["aaaaaaaaaa", "bbbbbbbbbb"])
            } else {
                page("raffi", "head", 3, 3, true, &["cccccccccc"])
            })
        };
        let listing = stable_listing(&mut fetch).unwrap();
        assert_eq!(listing.items.len(), 3);
        assert_eq!(listing.total, 3);
        assert_eq!(listing.head, "head");
        assert_eq!(listing.next, 3);

        // An unstable page restarts the whole scan, once.
        let mut attempt = 0;
        let mut fetch = |tab: &str, _start: usize| {
            if tab.is_empty() {
                attempt += 1;
            }
            Ok(page("raffi", "head", 1, 1, attempt >= 2, &["aaaaaaaaaa"]))
        };
        let listing = stable_listing(&mut fetch).unwrap();
        assert_eq!(listing.items.len(), 1);
        assert_eq!(attempt, 2, "the scan was retried exactly once");

        // A head or a total that moves between pages is just as unstable.
        for (heads, totals) in [(("first", "second"), (2, 2)), (("head", "head"), (2, 3))] {
            let mut fetch = |_tab: &str, start: usize| {
                let (head, total) = if start == 0 {
                    (heads.0, totals.0)
                } else {
                    (heads.1, totals.1)
                };
                Ok(page(
                    "raffi",
                    head,
                    total,
                    if start == 0 { 1 } else { total },
                    true,
                    &["a"],
                ))
            };
            let error = stable_listing(&mut fetch).unwrap_err();
            assert!(error.message.contains("kept changing"), "{error}");
            assert_eq!(error.code, Code::Io);
        }

        // A page that does not move forward cannot be joined with the rest.
        let mut fetch = |_tab: &str, _start: usize| Ok(page("raffi", "head", 3, 0, true, &["a"]));
        let error = stable_listing(&mut fetch).unwrap_err();
        assert!(error.message.contains("inconsistent"), "{error}");
        assert_eq!(error.code, Code::Io);
    }

    #[test]
    fn test_fit_measures_the_string_not_the_characters() {
        // `#` and a variation selector form one two-column glyph, so only
        // the glyph itself and the ellipsis fit in three columns.
        assert_eq!(fit("#\u{FE0F}aX", 3), "#\u{FE0F}…");
        let wide = "界".repeat(8);
        for text in [
            "#\u{FE0F}aX",
            "\u{2764}\u{FE0F}\u{2764}\u{FE0F}",
            "👩\u{200D}👩\u{200D}👧\u{200D}👦",
            wide.as_str(),
            "e\u{301}e\u{301}e\u{301}",
        ] {
            for budget in 1..=6 {
                let cut = fit(text, budget);
                assert!(cut.width() <= budget, "{text:?} in {budget}: {cut:?}");
                assert!(cut.ends_with('…') || cut == text, "{text:?}: {cut:?}");
            }
        }
        assert_eq!(fit("界界界", 5), "界界…");
        assert_eq!(fit("e\u{301}e\u{301}e\u{301}", 2), "e\u{301}…");
    }

    #[test]
    fn test_remaining_time_uses_a_readable_scale() {
        assert_eq!(remaining(0), "0s");
        assert_eq!(remaining(45), "45s");
        assert_eq!(remaining(60), "1m");
        assert_eq!(remaining(185), "3m");
        assert_eq!(remaining(3 * 60 * 60), "3h");
        assert_eq!(remaining(2 * 24 * 60 * 60), "2d");
        let mut temporary = item("a81be0c44f", ItemKind::Text, "a one-time code");
        temporary.expires_at = Some(NOW + 180);
        let row = TableRow::of(&temporary, Some(1), Clock::at(NOW), None);
        assert_eq!(row.when, "⏱ 3m");
    }

    #[test]
    fn test_image_rows_show_their_format_size_and_pixels() {
        let (mut store, dir) = temp_store("image-rows");
        let mut png = Vec::new();
        image::RgbaImage::new(64, 32)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        store
            .insert(
                Capture {
                    formats: vec![("image/png".into(), png)],
                    copied_at: 1,
                    source: None,
                    password_marked: false,
                },
                &LIMITS,
                NOW,
            )
            .unwrap();
        let settings = settings(3, Vec::new());
        let mut run = run(store, &settings);

        let json = list(
            &mut run,
            &ClipboardListArgs {
                json: true,
                ..Default::default()
            },
        )
        .unwrap();
        let value: serde_json::Value = serde_json::from_slice(
            String::from_utf8(json.stdout)
                .unwrap()
                .trim_end()
                .as_bytes(),
        )
        .unwrap();
        assert_eq!(value["kind"], "image");
        assert_eq!(value["width"], 64);
        assert_eq!(value["height"], 32);
        assert_eq!(value["preview"], "PNG image 64×32");

        let tsv = list(&mut run, &ClipboardListArgs::default()).unwrap();
        let tsv = String::from_utf8(tsv.stdout).unwrap();
        assert!(tsv.contains("PNG image 64×32"), "{tsv}");

        run.io.stdout_tty = true;
        let table = list(&mut run, &ClipboardListArgs::default()).unwrap();
        let table = String::from_utf8(table.stdout).unwrap();
        assert!(table.contains("PNG image 64×32 · "), "{table}");
        drop(run);
        let _ = std::fs::remove_dir_all(dir);
    }
}
