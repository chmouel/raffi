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
use std::io::{BufRead, Write};
use std::time::Duration;

use chrono::{NaiveDateTime, TimeZone};
use serde::Serialize;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::clipboard::{
    one_line, parse_uri_list, short_copy_time, Backend, ClipItem, CopyQ, History, ItemKind, Page,
    LINE_BREAK_MARK,
};
use super::clipboard_store::{self, kind_name, kind_predicate, now_secs, Store};
use crate::{
    ClipboardAddonConfig, ClipboardArgs, ClipboardClearArgs, ClipboardCommand, ClipboardKind,
    ClipboardListArgs, ClipboardShowArgs, ClipboardStatusArgs,
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
const ISO_TIME: &str = "%Y-%m-%dT%H:%M:%S";
const TEXT_FORMATS: [&str; 2] = ["text/plain", "text/plain;charset=utf-8"];

/// `raffi clipboard`, dispatched from `run()` once the config is loaded.
pub(crate) fn run_clipboard_cli(
    config: &ClipboardAddonConfig,
    args: &ClipboardArgs,
) -> Result<(), String> {
    let output = execute(config, args, Io::detect())?;
    let mut stdout = std::io::stdout().lock();
    stdout
        .write_all(&output.stdout)
        .and_then(|()| stdout.flush())
        .map_err(|error| format!("cannot write to stdout: {error}"))?;
    drop(stdout);
    for notice in &output.notices {
        eprintln!("{notice}");
    }
    Ok(())
}

/// What a command wrote: the bytes for stdout, and the lines for stderr.
#[derive(Debug, Default, PartialEq, Eq)]
struct Output {
    stdout: Vec<u8>,
    notices: Vec<String>,
}

impl Output {
    fn text(text: impl Into<String>) -> Self {
        Self {
            stdout: text.into().into_bytes(),
            notices: Vec::new(),
        }
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
    fn from_config(config: &ClipboardAddonConfig) -> Result<Self, String> {
        Ok(Self {
            min_chars: config.min_chars(),
            exclude: config.exclude_regexes()?,
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
    fn open(backend: Backend) -> Result<Self, String> {
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
    ) -> Result<Page, String> {
        let only_kind = kind.map_or("", ClipboardKind::name);
        match self {
            Self::Raffi(Some(store)) => {
                store.list_until(query, only_kind, min_chars, exclude, now, None)
            }
            Self::Raffi(None) => Ok(empty_page()),
            Self::CopyQ(copyq) => copyq_listing(copyq, query, only_kind, min_chars, exclude),
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
) -> Result<Page, String> {
    for _ in 0..2 {
        let mut items = Vec::new();
        let mut tab = String::new();
        let mut start = 0;
        let (total, stable) = loop {
            let page = copyq.list_page(&tab, start, query, only_kind, min_chars, exclude)?;
            tab = page.tab.clone();
            let (next, total, stable) = (page.next, page.total, page.stable);
            items.extend(page.items);
            if next >= total {
                break (total, stable);
            }
            if next <= start {
                return Err("CopyQ returned an inconsistent history listing".into());
            }
            start = next;
        };
        if stable {
            return Ok(Page {
                tab,
                total,
                head: String::new(),
                next: total,
                stable,
                items,
            });
        }
    }
    Err("the clipboard history kept changing, try again".into())
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
    fn eligible(&mut self) -> Result<Page, String> {
        self.search("", None, 0)
    }

    /// The listing `query` and `kind` select.
    fn search(
        &mut self,
        query: &str,
        kind: Option<ClipboardKind>,
        min_chars: usize,
    ) -> Result<Page, String> {
        let exclude = self.settings.exclude.clone();
        let now = self.clock.secs;
        self.clips.list(query, kind, min_chars, &exclude, now)
    }

    /// One snapshot of the eligible set, with the positions of the default
    /// listing within it. The positions come from a second backend listing
    /// with the configured `min_chars`, so the backend's own trimmed-length
    /// predicate decides what counts, and entries recorded since the snapshot
    /// are left out of the map.
    fn snapshot(&mut self) -> Result<(Page, HashMap<String, usize>), String> {
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
    fn resolve_one(
        &mut self,
        refs: &[String],
    ) -> Result<(ClipItem, Option<usize>, String), String> {
        let token = match refs {
            [token] => token,
            [] => return Err("no clipboard entry given".into()),
            _ => return Err("this command takes one clipboard entry".into()),
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
    ) -> Result<(Vec<ClipItem>, String), String> {
        if refs.is_empty() {
            return Err("no clipboard entry given".into());
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

fn execute(config: &ClipboardAddonConfig, args: &ClipboardArgs, io: Io) -> Result<Output, String> {
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
fn expand_refs(tokens: &[String]) -> Result<Vec<String>, String> {
    let mut refs = Vec::new();
    for token in tokens {
        if token != "-" {
            refs.push(token.clone());
            continue;
        }
        for line in std::io::stdin().lock().lines() {
            let line = line.map_err(|error| format!("cannot read the references: {error}"))?;
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

fn parse_ref(token: &str) -> Result<Ref, String> {
    if token == "-" {
        return Ok(Ref::Stdin);
    }
    if token.is_empty() {
        return Err("an empty reference names no clipboard entry".into());
    }
    if token.len() <= MAX_INDEX_DIGITS && token.bytes().all(|byte| byte.is_ascii_digit()) {
        let index = token
            .parse()
            .map_err(|_| format!("`{token}` is not a clipboard index"))?;
        return Ok(Ref::Index(index));
    }
    if token.len() >= MIN_PREFIX && token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Ok(Ref::Prefix(token.to_ascii_lowercase()));
    }
    Err(format!(
        "`{}` is not a clipboard reference: use an index of up to \
         {MAX_INDEX_DIGITS} digits, at least {MIN_PREFIX} characters of an id, \
         or `-` to read references from the input",
        sanitize(token, Sanitize::Spaces)
    ))
}

/// The entry `reference` names. Indexes are only accepted where the command
/// is harmless: they change as soon as the history does.
fn resolve<'a>(
    reference: &Ref,
    eligible: &'a [ClipItem],
    index: &HashMap<String, usize>,
    allow_index: bool,
) -> Result<&'a ClipItem, String> {
    match reference {
        Ref::Stdin => Err("this command does not read references from the input".into()),
        Ref::Index(position) => {
            if !allow_index {
                return Err(format!(
                    "`{position}` is an index, which changes as the history moves; \
                     pass the id `raffi clipboard list` prints"
                ));
            }
            if *position == 0 {
                return Err("clipboard indexes start at 1".into());
            }
            eligible
                .iter()
                .find(|item| index.get(&item.id) == Some(position))
                .ok_or_else(|| {
                    format!(
                        "no clipboard entry at index {position} (the history lists {} entries)",
                        index.len()
                    )
                })
        }
        Ref::Prefix(prefix) => {
            let matches: Vec<&ClipItem> = eligible
                .iter()
                .filter(|item| item.id.starts_with(prefix.as_str()))
                .collect();
            match matches.as_slice() {
                [] => Err(format!("no clipboard entry matches `{prefix}`")),
                [only] => Ok(only),
                many => Err(format!(
                    "`{prefix}` matches {} entries: {}",
                    many.len(),
                    candidates(many)
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
fn kind_filter(kind: Option<ClipboardKind>) -> Result<&'static str, String> {
    match kind {
        Some(kind) => kind_predicate(kind.name())
            .ok_or_else(|| format!("`{}` is not a clipboard kind", kind.name())),
        None => Ok("1"),
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
    expires_at: Option<i64>,
    preview: String,
}

fn row(item: &ClipItem, index: Option<usize>) -> Row {
    Row {
        index,
        id: item.id.clone(),
        kind: kind_name(item.kind),
        tier: item.tier,
        formats: item.formats.clone(),
        text_len: item.text_len,
        copied: item
            .copied
            .map(|copied| copied.format(ISO_TIME).to_string()),
        source: item.source.clone(),
        expires_at: item.expires_at,
        preview: preview_of(item, Sanitize::Spaces),
    }
}

/// The entry's label on one line, without anything that would drive a
/// terminal.
fn preview_of(item: &ClipItem, mode: Sanitize) -> String {
    let title = one_line(&item.title(), PREVIEW_CHARS);
    sanitize(&title.replace(LINE_BREAK_MARK, " "), mode)
}

fn json_lines(rows: &[Row]) -> Result<String, String> {
    let mut out = String::new();
    for row in rows {
        out.push_str(&serde_json::to_string(row).map_err(|error| error.to_string())?);
        out.push('\n');
    }
    Ok(out)
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
    let mut clean = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' => {}
            '\u{0}'..='\u{1f}' | '\u{7f}' => match mode {
                Sanitize::Spaces => clean.push(' '),
                Sanitize::Carets => {
                    clean.push('^');
                    clean.push(caret(character));
                }
            },
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
/// dropped. Wide characters take two columns, combining marks none.
fn fit(text: &str, budget: usize) -> String {
    if text.width() <= budget {
        return text.to_string();
    }
    let mut cut = String::new();
    let mut used = 0;
    for character in text.chars() {
        let width = character.width().unwrap_or(0);
        if used + width > budget.saturating_sub(1) {
            break;
        }
        used += width;
        cut.push(character);
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
    fn of(item: &ClipItem, index: Option<usize>, clock: Clock) -> Self {
        Self {
            index: index.map_or_else(|| "-".to_string(), |index| index.to_string()),
            id: short_id(&item.id),
            when: when_of(item, clock),
            kind: kind_name(item.kind).to_string(),
            preview: preview_of(item, Sanitize::Carets),
        }
    }
}

/// The copy time, with how long a temporary entry has left.
fn when_of(item: &ClipItem, clock: Clock) -> String {
    let mut when = item
        .copied
        .map_or_else(String::new, |copied| short_copy_time(copied, clock.local));
    if let Some(deadline) = item.expires_at {
        if !when.is_empty() {
            when.push(' ');
        }
        when.push_str(&format!(
            "⏱ {}s",
            deadline.saturating_sub(clock.secs).max(0)
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

fn list(run: &mut Run, args: &ClipboardListArgs) -> Result<Output, String> {
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
    let mut items = selected(&page, &eligible_ids, &index);
    if let Some(limit) = args.limit {
        items.truncate(limit);
    }
    if args.json {
        let rows: Vec<Row> = items
            .iter()
            .map(|(item, index)| row(item, *index))
            .collect();
        return Ok(Output::text(json_lines(&rows)?));
    }
    if run.io.stdout_tty {
        let rows: Vec<TableRow> = items
            .iter()
            .map(|(item, index)| TableRow::of(item, *index, run.clock))
            .collect();
        Ok(Output::text(table(&rows, run.io.width, run.io.color)))
    } else {
        let rows: Vec<Row> = items
            .iter()
            .map(|(item, index)| row(item, *index))
            .collect();
        Ok(Output::text(tsv(&rows)))
    }
}

fn show(run: &mut Run, args: &ClipboardShowArgs, refs: &[String]) -> Result<Output, String> {
    let (item, index, tab) = run.resolve_one(refs)?;
    match &run.clips {
        Clips::CopyQ(copyq) => show_copyq(copyq, &item, &tab, args, index),
        Clips::Raffi(Some(store)) => show_raffi(store, &item, args, run, index),
        Clips::Raffi(None) => Err("no clipboard history".into()),
    }
}

fn show_raffi(
    store: &Store,
    item: &ClipItem,
    args: &ClipboardShowArgs,
    run: &Run,
    index: Option<usize>,
) -> Result<Output, String> {
    let formats = store.formats(&item.id, run.clock.secs)?;
    if args.formats {
        return formats_output(&sizes_of(&formats), args.json);
    }
    if let Some(mime) = &args.mime {
        return match formats.iter().find(|(name, _)| name == mime) {
            Some((_, bytes)) => Ok(Output::bytes(bytes.clone())),
            None => Err(format!(
                "the entry holds no {} format, only {}",
                sanitize(mime, Sanitize::Spaces),
                names(&formats)
            )),
        };
    }
    if args.json {
        let text = match item.kind {
            ItemKind::Image => None,
            _ => exact_text(&formats).map(|bytes| String::from_utf8_lossy(&bytes).into_owned()),
        };
        return Ok(Output::text(metadata_line(item, index, text.as_deref())?));
    }
    match item.kind {
        ItemKind::Image => {
            let (mime, bytes) = preferred_image(&formats)
                .ok_or_else(|| "the entry holds no image format".to_string())?;
            if run.io.stdout_tty {
                return Err(format!(
                    "entry {} is a {} image ({}); redirect its output or pass --mime",
                    index.map_or_else(|| short_id(&item.id), |index| index.to_string()),
                    sanitize(&mime["image/".len()..], Sanitize::Spaces).to_uppercase(),
                    super::preview::format_byte_size(bytes.len() as u64)
                ));
            }
            Ok(Output::bytes(bytes))
        }
        _ => exact_text(&formats)
            .map(Output::bytes)
            .ok_or_else(|| format!("the entry holds no text to show, only {}", names(&formats))),
    }
}

fn show_copyq(
    copyq: &CopyQ,
    item: &ClipItem,
    tab: &str,
    args: &ClipboardShowArgs,
    index: Option<usize>,
) -> Result<Output, String> {
    if args.mime.is_some() {
        return Err("--mime is not supported by the copyq backend".into());
    }
    if args.formats {
        let preview = copyq.preview(&item.reference(tab))?;
        return formats_output(&preview.formats, args.json);
    }
    if args.json {
        let text = match item.kind {
            ItemKind::Image => None,
            _ => Some(
                copyq
                    .full_text(&item.reference(tab))
                    .map_err(|error| copyq_text_error(&error))?,
            ),
        };
        return Ok(Output::text(metadata_line(item, index, text.as_deref())?));
    }
    if item.kind == ItemKind::Image {
        return Err("exporting an image is not supported by the copyq backend".into());
    }
    let text = copyq
        .full_text(&item.reference(tab))
        .map_err(|error| copyq_text_error(&error))?;
    Ok(Output::text(text))
}

fn copyq_text_error(error: &str) -> String {
    if error.contains("longer than") {
        return format!(
            "the entry is larger than {}; export it with CopyQ",
            super::preview::format_byte_size(clipboard_store::MAX_TEXT_BYTES as u64)
        );
    }
    error.to_string()
}

/// The formats of an entry with their size, for `show --formats`.
fn formats_output(formats: &[(String, usize)], json: bool) -> Result<Output, String> {
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
        let mut out =
            serde_json::to_string(&listing).map_err(|error| format!("cannot format: {error}"))?;
        out.push('\n');
        return Ok(Output::text(out));
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
            super::preview::format_byte_size(*bytes as u64)
        ));
    }
    Ok(Output::text(out))
}

fn metadata_line(
    item: &ClipItem,
    index: Option<usize>,
    text: Option<&str>,
) -> Result<String, String> {
    let mut value = serde_json::to_value(row(item, index))
        .map_err(|error| format!("cannot format the entry: {error}"))?;
    if let Some(text) = text {
        value["text"] = serde_json::Value::String(text.to_string());
    }
    let mut out = serde_json::to_string(&value)
        .map_err(|error| format!("cannot format the entry: {error}"))?;
    out.push('\n');
    Ok(out)
}

fn sizes_of(formats: &[(String, Vec<u8>)]) -> Vec<(String, usize)> {
    formats
        .iter()
        .map(|(mime, bytes)| (mime.clone(), bytes.len()))
        .collect()
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

fn copy(run: &mut Run, refs: &[String]) -> Result<Output, String> {
    let (item, _, tab) = run.resolve_one(refs)?;
    match &run.clips {
        Clips::CopyQ(copyq) => copyq.select(&item.reference(&tab))?,
        Clips::Raffi(_) => super::cache::clipboard_select(&item.id)?,
    }
    Ok(Output::default())
}

fn delete(run: &mut Run, refs: &[String]) -> Result<Output, String> {
    let (items, tab) = run.resolve_all(refs, false)?;
    let count = items.len();
    match &mut run.clips {
        Clips::CopyQ(copyq) => {
            for item in &items {
                copyq.delete(&item.reference(&tab))?;
            }
            Ok(deleted(count, run.io, true))
        }
        Clips::Raffi(None) => Err("no clipboard history".into()),
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

fn clear(run: &mut Run, args: &ClipboardClearArgs) -> Result<Output, String> {
    if run.clips.is_copyq() {
        return Err(
            "clear is only supported by the raffi backend; use CopyQ's own interface".into(),
        );
    }
    let filter = kind_filter(args.kind)?;
    let eligible = run.search("", args.kind, 0)?;
    let (total, alive) = match &run.clips {
        Clips::Raffi(Some(store)) => store.count(filter, run.clock.secs)?,
        _ => return Err("no clipboard history".into()),
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
        _ => return Err("no clipboard history".into()),
    };
    Ok(deleted(removed, run.io, emptied))
}

/// Ask before deleting everything. Only a terminal can answer, so a script
/// has to pass `--yes` and be explicit about it.
fn confirm(prompt: &str, io: Io) -> Result<(), String> {
    if !io.stdin_tty {
        return Err("clear needs --yes when its input is not a terminal".into());
    }
    eprint!("{prompt} [y/N] ");
    std::io::stderr()
        .flush()
        .map_err(|error| format!("cannot write to stderr: {error}"))?;
    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .map_err(|error| format!("cannot read the answer: {error}"))?;
    if matches!(answer.trim(), "y" | "Y" | "yes" | "Yes" | "YES") {
        Ok(())
    } else {
        Err("nothing was deleted".into())
    }
}

fn status(run: &mut Run, args: &ClipboardStatusArgs) -> Result<Output, String> {
    let recorder = match &run.clips {
        Clips::Raffi(_) => super::cache::clipboard_recording(),
        Clips::CopyQ(_) => {
            super::clipboard_history::copyq_running(super::clipboard_history::running_cmdlines)
        }
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
        backend: match run.settings.backend {
            crate::ClipboardBackend::Auto => format!("{} (auto)", run.clips.name()),
            _ => run.clips.name().to_string(),
        },
        recorder,
        database,
        database_bytes,
        entries: counts.alive,
        temporary: counts.temporary,
        limits: run.settings.limits.into(),
    };
    if args.json {
        let mut out = serde_json::to_string(&status)
            .map_err(|error| format!("cannot format the status: {error}"))?;
        out.push('\n');
        return Ok(Output::text(out));
    }
    Ok(Output::text(status_text(&status)))
}

/// What `status` reports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct Status {
    backend: String,
    /// Whether a service records the clipboard for that backend.
    recorder: bool,
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
    let service = if status.backend.starts_with("raffi") {
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
            format!("{path} ({})", super::preview::format_byte_size(bytes))
        }
        (Some(path), None) => path.clone(),
        (None, _) => "not used by this backend".to_string(),
    };
    let entries = match status.temporary {
        0 => status.entries.to_string(),
        temporary => format!("{} ({temporary} temporary)", status.entries),
    };
    report(&[
        ("Backend", status.backend.clone()),
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
        out.push_str(&format!(
            "{}{}{}\n",
            label,
            " ".repeat(padding),
            sanitize(value, Sanitize::Spaces)
        ));
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
        let refused = resolve(&newest, &entries, &index, false).unwrap_err();
        assert!(
            refused.contains("changes as the history moves"),
            "{refused}"
        );
        assert!(resolve(&Ref::Index(0), &entries, &index, true)
            .unwrap_err()
            .contains("start at 1"));
        assert!(resolve(&Ref::Index(2), &entries, &index, true)
            .unwrap_err()
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
        let ambiguous = resolve(&Ref::Prefix("3fa9c1".into()), &entries, &index, true).unwrap_err();
        assert!(ambiguous.contains("matches 2 entries"), "{ambiguous}");
        assert!(ambiguous.contains("3fa9c1d2e0"), "{ambiguous}");
        // The candidate titles cannot drive the terminal.
        assert!(!ambiguous.contains('\u{1b}'), "{ambiguous}");
        assert!(ambiguous.contains("]52;c;"), "{ambiguous}");
        assert!(
            resolve(&Ref::Prefix("deadbeef".into()), &entries, &index, true)
                .unwrap_err()
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
        let out_of_range = run.resolve_one(&["9".into()]).unwrap_err();
        assert!(out_of_range.contains("at index 9"), "{out_of_range}");
        assert!(run
            .resolve_one(&["ffffffffffffffff".into()])
            .unwrap_err()
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
        let copied = chrono::Local
            .timestamp_opt(2, 0)
            .single()
            .map(|time| time.naive_local())
            .unwrap();
        assert_eq!(fields[0][2], copied.format(ISO_TIME).to_string());

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
    fn test_json_lines_carry_every_field() {
        let mut one = item("3fa9c1d2e0", ItemKind::Text, "git push");
        one.formats = vec!["text/plain".into()];
        one.copied = chrono::Local
            .timestamp_opt(NOW, 0)
            .single()
            .map(|time| time.naive_local());
        one.expires_at = Some(NOW + 45);
        let rows = [
            row(&one, Some(1)),
            row(&item("a81be0c44f", ItemKind::Image, ""), None),
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
        assert!(parsed[1]["index"].is_null());
        assert_eq!(parsed[1]["kind"], "image");
        assert!(parsed[1]["copied"].is_null());
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
        let lines = tsv(&[row(&entry, Some(1))]);
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
        temporary.copied = chrono::Local
            .timestamp_opt(NOW - 60, 0)
            .single()
            .map(|time| time.naive_local());
        temporary.expires_at = Some(NOW + 45);
        let rows = [
            TableRow::of(&temporary, Some(2), Clock::at(NOW)),
            TableRow::of(
                &item("77aa01fe3c", ItemKind::Image, ""),
                None,
                Clock::at(NOW),
            ),
        ];
        assert_eq!(
            rows[0].when,
            format!(
                "{} ⏱ 45s",
                short_copy_time(temporary.copied.unwrap(), Clock::at(NOW).local)
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
        let refused = show(&mut run, &args, std::slice::from_ref(&id)).unwrap_err();
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
        let error = show(&mut run, &missing, std::slice::from_ref(&id)).unwrap_err();
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
        let refused = show(&mut run, &args, std::slice::from_ref(&id)).unwrap_err();
        assert!(refused.contains("redirect its output"), "{refused}");
        assert!(refused.contains("PNG"), "{refused}");
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
            backend: "raffi (auto)".into(),
            recorder: true,
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
        assert_eq!(json["backend"], "raffi (auto)");
        assert_eq!(json["recorder"], true);
        assert_eq!(json["entries"], 842);
        assert_eq!(json["max_items"], 100);

        // A backend that keeps its own storage has no path or size to show.
        let copyq = Status {
            backend: "copyq".into(),
            recorder: false,
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
        assert!(json["database"].is_null());
        assert!(json["database_bytes"].is_null());
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
}
