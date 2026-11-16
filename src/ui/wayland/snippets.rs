use std::cmp::Reverse;
use std::fs::File;
use std::io::Read;
use std::process::{Command, Stdio};
use std::time::Duration;

use chrono::{DateTime, Local};
use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;
use iced::Task;
use serde::Deserialize;

use super::actions::{read_clipboard, run_bounded};
use super::clipboard_history::{self, Backend};
use super::placeholders::{self, ClipboardNeeds, Expanded, Mode, Providers, MAX_VALUE};
use super::state::{ClipboardData, Message};
use crate::{ClipboardHistory, TextSnippet};

const SHELL_TIMEOUT: Duration = Duration::from_secs(5);
const PREVIEW_CHARS: usize = 80;

#[derive(Debug, PartialEq)]
pub(super) enum SnippetIcon {
    File(String),
    Glyph(String),
}

impl super::state::LauncherApp {
    pub(super) fn snippet_icon(&self, snippet: &TextSnippet) -> Option<SnippetIcon> {
        if self.no_icons {
            return None;
        }
        snippet
            .icon
            .iter()
            .chain(self.text_snippets.icon.iter())
            .find_map(|icon| self.resolve_icon_or_glyph(icon))
    }

    /// Resolve an icon name or image path to a file, or accept a literal
    /// emoji/glyph. Returns `None` when icons are disabled.
    pub(super) fn resolve_icon_or_glyph(&self, icon: &str) -> Option<SnippetIcon> {
        if self.no_icons {
            return None;
        }
        if let Some(path) = crate::resolve_icon_reference(icon, &self.icon_map)
            .filter(|path| std::path::Path::new(path).is_file())
        {
            return Some(SnippetIcon::File(path));
        }
        // Literal glyphs may include variation selectors or joined emoji.
        if icon.chars().any(|c| !c.is_ascii() && !c.is_alphanumeric())
            && !icon.contains('/')
            && !icon.chars().any(char::is_whitespace)
        {
            return Some(SnippetIcon::Glyph(icon.to_string()));
        }
        None
    }

    /// Icon of a keyword or help row: the theme icon or glyph it names,
    /// else an emoji standing in for a theme icon the theme lacks.
    pub(super) fn keyword_icon(&self, icon: &str) -> Option<SnippetIcon> {
        if self.no_icons {
            return None;
        }
        self.resolve_icon_or_glyph(icon).or_else(|| {
            super::app::fallback_emoji(icon).map(|glyph| SnippetIcon::Glyph(glyph.to_string()))
        })
    }
}

#[derive(Debug, Deserialize)]
struct SnippetCommandItem {
    title: String,
    arg: Option<String>,
    keyword: Option<String>,
    icon: Option<super::types::ScriptFilterIcon>,
}

#[derive(Debug, Deserialize)]
struct SnippetCommandResponse {
    items: Vec<SnippetCommandItem>,
}

pub(super) fn execute_text_snippet_command(
    command: String,
    args: Vec<String>,
    generation: u64,
) -> Task<Message> {
    Task::perform(
        async move { run_text_snippet_command(&command, &args) },
        move |result| Message::TextSnippetCommandResult(generation, result),
    )
}

/// Run a snippet `command` and parse its items. Blocking.
pub(super) fn run_text_snippet_command(
    command: &str,
    args: &[String],
) -> Result<Vec<TextSnippet>, String> {
    crate::debug_log!("snippets: executing command={command:?} args={args:?}");
    let output = Command::new(command)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();

    match output {
        Ok(output) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            match serde_json::from_str::<SnippetCommandResponse>(&stdout) {
                Ok(response) => {
                    crate::debug_log!(
                        "snippets: got {} items from {command:?}",
                        response.items.len()
                    );
                    Ok(response
                        .items
                        .into_iter()
                        .map(|item| TextSnippet {
                            name: item.title,
                            value: item.arg.unwrap_or_default(),
                            keyword: item.keyword,
                            icon: item.icon.and_then(|icon| icon.path),
                        })
                        .collect())
                }
                Err(error) => Err(format!("Invalid JSON: {}", error)),
            }
        }
        Ok(output) => Err(format!("Command exited with status {}", output.status)),
        Err(error) => Err(format!("Failed to execute: {}", error)),
    }
}

/// Snippets of a YAML `file` source. Blocking.
pub(super) fn read_snippet_file(path: &str) -> Result<Vec<TextSnippet>, String> {
    let contents =
        std::fs::read_to_string(path).map_err(|error| format!("cannot read {path}: {error}"))?;
    serde_yaml::from_str::<Vec<TextSnippet>>(&contents)
        .map_err(|error| format!("invalid YAML in {path}: {error}"))
}

/// Snippets of the `.snippet` files in a `directory` source, by name.
/// Blocking.
pub(super) fn read_snippet_dir(path: &str) -> Result<Vec<TextSnippet>, String> {
    let entries = std::fs::read_dir(path)
        .map_err(|error| format!("cannot read directory {path}: {error}"))?;
    let mut snippets: Vec<TextSnippet> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("snippet"))
        .filter_map(|path| {
            std::fs::read_to_string(&path)
                .ok()
                .and_then(|contents| parse_snippet_file(&contents))
        })
        .collect();
    snippets.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(snippets)
}

/// Snippets of a markdown `markdown` source. A missing file is an empty
/// source, so the first save creates it. Blocking.
pub(super) fn read_snippet_markdown(path: &str) -> Result<Vec<TextSnippet>, String> {
    match std::fs::read_to_string(path) {
        Ok(contents) => Ok(parse_markdown_snippets(&contents)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(format!("cannot read {path}: {error}")),
    }
}

/// Whether `line` is a heading once its escaping backslashes are dropped.
fn is_heading_like(line: &str) -> bool {
    line.trim_start_matches('\\').starts_with("## ")
}

/// Parse `## Name` sections. Each section may start with a `keyword:` line
/// and one blank line; the body runs to the next heading, without the blank
/// lines that end it. Body lines that would read as headings are written with
/// one extra leading backslash, which is removed again here.
pub(super) fn parse_markdown_snippets(contents: &str) -> Vec<TextSnippet> {
    let mut snippets = Vec::new();
    let mut current: Option<(String, Option<String>, Vec<String>)> = None;
    // 0: a keyword or blank line may follow the heading, 1: only a blank
    // line may follow, 2: the body.
    let mut phase = 0u8;
    for line in contents.lines() {
        if let Some(name) = line.strip_prefix("## ") {
            if let Some(done) = current.take() {
                push_markdown_section(done, &mut snippets);
            }
            current = Some((name.trim().to_string(), None, Vec::new()));
            phase = 0;
            continue;
        }
        let Some((_, keyword, body)) = current.as_mut() else {
            continue;
        };
        if phase < 2 {
            if phase == 0 {
                if let Some(value) = line.strip_prefix("keyword:") {
                    *keyword = Some(value.trim().to_string());
                    phase = 1;
                    continue;
                }
            }
            phase = 2;
            if line.is_empty() {
                continue;
            }
        }
        let unescaped = if line.starts_with('\\') && is_heading_like(line) {
            &line[1..]
        } else {
            line
        };
        body.push(unescaped.to_string());
    }
    if let Some(done) = current {
        push_markdown_section(done, &mut snippets);
    }
    snippets
}

fn push_markdown_section(
    (name, keyword, mut body): (String, Option<String>, Vec<String>),
    snippets: &mut Vec<TextSnippet>,
) {
    while body.last().is_some_and(|line| line.is_empty()) {
        body.pop();
    }
    if name.is_empty() || body.is_empty() {
        return;
    }
    snippets.push(TextSnippet {
        name,
        value: body.join("\n"),
        keyword: keyword.filter(|keyword| !keyword.is_empty()),
        icon: None,
    });
}

/// The markdown section of `snippet`, ending in a newline.
pub(super) fn render_markdown_snippet(snippet: &TextSnippet) -> String {
    let mut out = format!("## {}\n", snippet.name);
    if let Some(keyword) = &snippet.keyword {
        out.push_str(&format!("keyword: {keyword}\n"));
    }
    out.push('\n');
    for line in snippet.value.trim_end_matches('\n').split('\n') {
        if is_heading_like(line) {
            out.push('\\');
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Create `dir` and its missing parents, private to the user when new.
fn create_private_dir(dir: &std::path::Path) -> Result<(), String> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|error| format!("cannot create {}: {error}", dir.display()))
}

/// Append `snippet` to the markdown file at `path`, creating the file (mode
/// 0600) and its folder when needed. Refuses a name that is already in the
/// file. The section goes out in one append write, so an existing file keeps
/// its mode and a failed write never leaves a temporary file. Blocking.
pub(super) fn append_markdown_snippet(path: &str, snippet: &TextSnippet) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let existing = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(format!("cannot read {path}: {error}")),
    };
    if parse_markdown_snippets(&existing)
        .iter()
        .any(|saved| saved.name == snippet.name)
    {
        return Err(format!("A snippet named '{}' already exists", snippet.name));
    }
    let mut section = String::new();
    if !existing.is_empty() {
        if !existing.ends_with('\n') {
            section.push('\n');
        }
        if !existing.ends_with("\n\n") {
            section.push('\n');
        }
    }
    section.push_str(&render_markdown_snippet(snippet));
    if let Some(parent) = std::path::Path::new(path).parent() {
        if !parent.as_os_str().is_empty() {
            create_private_dir(parent)?;
        }
    }
    std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .open(path)
        .and_then(|mut file| file.write_all(section.as_bytes()))
        .map_err(|error| format!("cannot write {path}: {error}"))
}

/// Where a clipboard entry can be saved for a source: the directory of
/// `.snippet` files or the markdown file that the source is read from. `None`
/// when the source is read from somewhere else, since a save would not show.
pub(super) fn snippet_save_path(config: &crate::TextSnippetSourceConfig) -> Option<&str> {
    if config.snippets.is_some() || config.file.is_some() {
        return None;
    }
    config.directory.as_deref().or(config.markdown.as_deref())
}

/// File name stem of a snippet called `name`: lowercase letters and digits
/// joined by single dashes.
fn snippet_file_stem(name: &str) -> String {
    let mut stem = String::new();
    for c in name.chars().flat_map(char::to_lowercase) {
        if c.is_alphanumeric() {
            stem.push(c);
        } else if !stem.is_empty() && !stem.ends_with('-') {
            stem.push('-');
        }
    }
    let stem: String = stem.trim_end_matches('-').chars().take(60).collect();
    if stem.is_empty() {
        "snippet".into()
    } else {
        stem.trim_end_matches('-').to_string()
    }
}

/// The `.snippet` file text of `snippet`: name, optional `keyword:`, a `---`
/// line, then the value.
pub(super) fn render_snippet_file(snippet: &TextSnippet) -> String {
    let mut out = format!("{}\n", snippet.name);
    if let Some(keyword) = &snippet.keyword {
        out.push_str(&format!("keyword: {keyword}\n"));
    }
    out.push_str("---\n");
    out.push_str(snippet.value.trim_end_matches('\n'));
    out.push('\n');
    out
}

/// Attempts at a free `-N` file name before a directory save gives up.
const MAX_SAVE_NAME_ATTEMPTS: u32 = 1000;

/// A temporary file name in `dir` that no other save, process or leftover
/// shares. It has no `.snippet` extension, so a reader never sees it.
fn unique_temp_path(dir: &std::path::Path) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.subsec_nanos());
    dir.join(format!(
        ".raffi-save-{}-{}-{nanos}.tmp",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ))
}

/// Write `snippet` as a new `.snippet` file (mode 0600) in the directory at
/// `path`, creating the folder when needed. Refuses a name the directory
/// already has and never replaces a file: the text is written to a private
/// temporary file, then hard-linked to the first free name. Blocking.
pub(super) fn save_snippet_to_dir(path: &str, snippet: &TextSnippet) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    if snippet.name.trim().is_empty() || snippet.name.contains('\n') {
        return Err("The snippet name must be one non-empty line".into());
    }
    if snippet.value.trim().is_empty() {
        return Err("The snippet has no text".into());
    }
    let dir = std::path::Path::new(path);
    create_private_dir(dir)?;
    if read_snippet_dir(path)?
        .iter()
        .any(|saved| saved.name == snippet.name)
    {
        return Err(format!("A snippet named '{}' already exists", snippet.name));
    }
    let stem = snippet_file_stem(&snippet.name);
    let temp = unique_temp_path(dir);
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)
        .and_then(|mut file| file.write_all(render_snippet_file(snippet).as_bytes()));
    let published = written
        .map_err(|error| format!("cannot write {path}: {error}"))
        .and_then(|()| {
            for attempt in 1..=MAX_SAVE_NAME_ATTEMPTS {
                let target = if attempt == 1 {
                    dir.join(format!("{stem}.snippet"))
                } else {
                    dir.join(format!("{stem}-{attempt}.snippet"))
                };
                match std::fs::hard_link(&temp, &target) {
                    Ok(()) => return Ok(()),
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(error) => {
                        return Err(format!("cannot write {}: {error}", target.display()))
                    }
                }
            }
            Err(format!(
                "cannot find a free file name for '{stem}' in {path}"
            ))
        });
    let _ = std::fs::remove_file(&temp);
    published
}

/// Save `snippet` to the target of `config`. With placeholders enabled, the
/// value is escaped so the saved snippet pastes as copied. Blocking.
pub(super) fn save_snippet(
    config: &crate::TextSnippetSourceConfig,
    snippet: &TextSnippet,
) -> Result<(), String> {
    let escaped;
    let snippet = if config.placeholders {
        escaped = TextSnippet {
            value: placeholders::escape_literal(&snippet.value),
            ..snippet.clone()
        };
        &escaped
    } else {
        snippet
    };
    match (&config.directory, snippet_save_path(config)) {
        (Some(_), Some(path)) => save_snippet_to_dir(path, snippet),
        (None, Some(path)) => append_markdown_snippet(path, snippet),
        _ => Err("No directory or markdown snippet file is configured".into()),
    }
}

/// Every snippet of `config`, whatever kind of source it is. Blocking.
pub(super) fn load_snippet_source(
    config: &crate::TextSnippetSourceConfig,
) -> Result<Vec<TextSnippet>, String> {
    if let Some(snippets) = &config.snippets {
        Ok(snippets.clone())
    } else if let Some(file) = &config.file {
        read_snippet_file(file)
    } else if let Some(directory) = &config.directory {
        read_snippet_dir(directory)
    } else if let Some(markdown) = &config.markdown {
        read_snippet_markdown(markdown)
    } else if let Some(command) = &config.command {
        run_text_snippet_command(command, &config.args)
    } else {
        Ok(Vec::new())
    }
}

/// Parse a `.snippet` file: a name line, optional `keyword:` and `icon:` header
/// lines, a `---` separator, then the snippet value.
pub(super) fn parse_snippet_file(contents: &str) -> Option<TextSnippet> {
    let mut lines = contents.lines();
    let name = lines.next()?.trim().to_string();
    let mut keyword = None;
    let mut icon = None;
    loop {
        let line = lines.next()?.trim();
        if line == "---" {
            break;
        }
        if let Some(value) = line.strip_prefix("keyword:") {
            keyword = Some(value.trim().to_string());
        } else {
            icon = Some(line.strip_prefix("icon:")?.trim().to_string());
        }
    }
    let value = lines.collect::<Vec<_>>().join("\n");
    if value.is_empty() {
        return None;
    }
    Some(TextSnippet {
        name,
        value,
        keyword: keyword.filter(|keyword| !keyword.is_empty()),
        icon: icon.filter(|icon| !icon.is_empty()),
    })
}

/// Usage cache key of snippet `name` from the source triggered by `keyword`.
pub(super) fn snippet_mru_key(keyword: &str, name: &str) -> String {
    format!("{keyword}:{name}")
}

/// Rank snippets for `query`: exact keyword, keyword prefix, fuzzy name, then
/// fuzzy content. `usage[i]` (higher first) orders the empty query and breaks
/// ties, except for `!` queries, which look up a keyword and keep the original
/// order on ties.
pub(super) fn filter_snippets(snippets: &[TextSnippet], query: &str, usage: &[u64]) -> Vec<usize> {
    let usage_of = |index: usize| Reverse(usage.get(index).copied().unwrap_or(0));
    if query.is_empty() {
        let mut result: Vec<usize> = (0..snippets.len()).collect();
        result.sort_by_key(|&index| usage_of(index));
        return result;
    }
    let by_usage = !query.starts_with('!');
    let matcher = SkimMatcherV2::default();
    let lowered = query.to_lowercase();
    let mut ranked: Vec<(u8, i64, usize)> = snippets
        .iter()
        .enumerate()
        .filter_map(|(index, snippet)| {
            if let Some(keyword) = snippet.keyword.as_deref().map(str::to_lowercase) {
                if keyword == lowered {
                    return Some((0, 0, index));
                }
                if keyword.starts_with(&lowered) {
                    return Some((1, 0, index));
                }
            }
            if let Some(score) = matcher.fuzzy_match(&snippet.name, query) {
                return Some((2, score, index));
            }
            matcher
                .fuzzy_match(&snippet.value, query)
                .map(|score| (3, score, index))
        })
        .collect();
    ranked.sort_by_key(|&(tier, score, index)| {
        let usage = if by_usage {
            usage_of(index)
        } else {
            Reverse(0)
        };
        (tier, Reverse(score), usage)
    });
    let result: Vec<usize> = ranked.into_iter().map(|(_, _, index)| index).collect();
    crate::debug_log!("snippets: filter query={query:?} results={}", result.len());
    result
}

/// Collapse line breaks into single spaces and shorten a subtitle to
/// `PREVIEW_CHARS` characters without splitting a multibyte codepoint.
pub(super) fn truncate_preview(value: &str) -> String {
    let normalized;
    let source = if value.contains(['\n', '\r']) {
        normalized = value
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        normalized.as_str()
    } else {
        value
    };
    match source.char_indices().nth(PREVIEW_CHARS) {
        Some((cut, _)) => format!("{}...", &source[..cut]),
        None => source.to_string(),
    }
}

/// Clock, environment, clipboard, shell and randomness for real expansion.
pub(super) struct LiveProviders {
    clipboard: ClipboardData,
    /// `None` for previews, which only use already fetched clipboard data.
    history_setting: Option<ClipboardHistory>,
    history_backend: Option<Result<Backend, String>>,
    random: Option<File>,
}

impl LiveProviders {
    /// Providers for subtitles: the clipboard comes from an earlier fetch and is
    /// never read here, since this runs on the UI thread.
    pub(super) fn preview(clipboard: &ClipboardData) -> Self {
        Self {
            clipboard: clipboard.clone(),
            history_setting: None,
            history_backend: None,
            random: None,
        }
    }

    /// Providers that read the clipboard and its history (once each) when a
    /// placeholder needs them.
    pub(super) fn live(history_setting: ClipboardHistory) -> Self {
        Self {
            clipboard: ClipboardData::default(),
            history_setting: Some(history_setting),
            history_backend: None,
            random: None,
        }
    }
}

impl Providers for LiveProviders {
    type Tz = Local;

    fn now(&mut self) -> DateTime<Local> {
        Local::now()
    }

    fn clipboard(&mut self) -> Result<String, String> {
        if self.history_setting.is_none() {
            return self.clipboard.current.clone().unwrap_or_else(not_loaded);
        }
        self.clipboard
            .current
            .get_or_insert_with(read_clipboard)
            .clone()
    }

    fn clipboard_history(&mut self, index: usize) -> Result<String, String> {
        if let Some(entry) = self.clipboard.history.get(&index) {
            return entry.clone();
        }
        let Some(setting) = self.history_setting else {
            return not_loaded();
        };
        let entry = match self
            .history_backend
            .get_or_insert_with(|| clipboard_history::resolve(setting))
        {
            Ok(backend) => clipboard_history::read_entries(backend, &[index])
                .remove(&index)
                .unwrap_or_else(not_loaded),
            Err(error) => Err(error.clone()),
        };
        self.clipboard.history.insert(index, entry.clone());
        entry
    }

    fn env(&mut self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }

    fn shell(&mut self, command: &str) -> Result<String, String> {
        let mut process = Command::new("sh");
        process.arg("-c").arg(command);
        let output = run_bounded(process, SHELL_TIMEOUT, MAX_VALUE)?;
        let mut text = String::from_utf8_lossy(&output).into_owned();
        if text.ends_with('\n') {
            text.pop();
        }
        Ok(text)
    }

    fn random_u64(&mut self) -> Result<u64, String> {
        if self.random.is_none() {
            self.random = Some(
                File::open("/dev/urandom")
                    .map_err(|error| format!("cannot open /dev/urandom: {error}"))?,
            );
        }
        let mut bytes = [0u8; 8];
        self.random
            .as_mut()
            .map(|file| file.read_exact(&mut bytes))
            .transpose()
            .map_err(|error| format!("cannot read /dev/urandom: {error}"))?;
        Ok(u64::from_ne_bytes(bytes))
    }
}

/// Subtitle text for every snippet, indexed like `snippets`. `None` means the
/// raw value should be shown (expansion failed or is not ready yet).
pub(super) fn compute_previews(
    snippets: &[TextSnippet],
    allow_shell: bool,
    clipboard: &ClipboardData,
) -> Vec<Option<String>> {
    let mut providers = LiveProviders::preview(clipboard);
    snippets
        .iter()
        .map(|snippet| {
            placeholders::expand(
                &snippet.value,
                &mut providers,
                snippets,
                allow_shell,
                Mode::Preview,
            )
            .ok()
            .map(|expanded| expanded.text)
        })
        .collect()
}

fn not_loaded() -> Result<String, String> {
    Err("clipboard not loaded yet".into())
}

/// Clipboard reads the previews of `snippets` need that `fetched` lacks.
pub(super) fn missing_clipboard(
    snippets: &[TextSnippet],
    fetched: &ClipboardData,
) -> ClipboardNeeds {
    let mut needs = ClipboardNeeds::default();
    for snippet in snippets {
        placeholders::clipboard_needs(&snippet.value, snippets, &mut needs);
    }
    needs.current &= fetched.current.is_none();
    needs
        .history
        .retain(|index| !fetched.history.contains_key(index));
    needs
}

fn read_clipboard_data(needs: &ClipboardNeeds, setting: ClipboardHistory) -> ClipboardData {
    let current = needs.current.then(read_clipboard);
    let indices: Vec<usize> = needs.history.iter().copied().collect();
    let history = if indices.is_empty() {
        Default::default()
    } else {
        match clipboard_history::resolve(setting) {
            Ok(backend) => clipboard_history::read_entries(&backend, &indices),
            Err(error) => failed_clipboard(needs, &error).history,
        }
    };
    ClipboardData { current, history }
}

/// Mark every needed read as failed, so it is not requested again.
fn failed_clipboard(needs: &ClipboardNeeds, error: &str) -> ClipboardData {
    ClipboardData {
        current: needs.current.then(|| Err(error.to_string())),
        history: needs
            .history
            .iter()
            .map(|&index| (index, Err(error.to_string())))
            .collect(),
    }
}

/// Run blocking work on its own thread so subprocesses never stall the UI.
async fn run_blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    let (sender, receiver) = iced::futures::channel::oneshot::channel();
    std::thread::spawn(move || {
        let _ = sender.send(work());
    });
    receiver
        .await
        .unwrap_or_else(|_| Err("snippet worker stopped unexpectedly".into()))
}

pub(super) fn fetch_clipboard(
    session: u64,
    needs: ClipboardNeeds,
    setting: ClipboardHistory,
) -> Task<Message> {
    let requested = needs.clone();
    Task::perform(
        run_blocking(move || Ok(read_clipboard_data(&needs, setting))),
        move |result| {
            let data = result.unwrap_or_else(|error| failed_clipboard(&requested, &error));
            Message::TextSnippetClipboard(session, data)
        },
    )
}

pub(super) fn expand_final(
    template: String,
    snippets: Vec<TextSnippet>,
    allow_shell: bool,
    session: u64,
    history_setting: ClipboardHistory,
) -> Task<Message> {
    Task::perform(
        run_blocking(move || {
            placeholders::expand(
                &template,
                &mut LiveProviders::live(history_setting),
                &snippets,
                allow_shell,
                Mode::Final,
            )
        }),
        move |result: Result<Expanded, String>| Message::TextSnippetExpanded(session, result),
    )
}

/// A uniquely named temporary directory for tests, removed on drop.
#[cfg(test)]
pub(super) struct TestDir(std::path::PathBuf);

#[cfg(test)]
impl TestDir {
    pub(super) fn new(name: &str) -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "raffi-{name}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    pub(super) fn path(&self) -> &std::path::Path {
        &self.0
    }
}

#[cfg(test)]
impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        append_markdown_snippet, compute_previews, filter_snippets, missing_clipboard,
        parse_markdown_snippets, parse_snippet_file, read_snippet_markdown,
        render_markdown_snippet, save_snippet, save_snippet_to_dir, snippet_save_path,
        truncate_preview, TestDir,
    };
    use crate::ui::wayland::state::ClipboardData;
    use crate::TextSnippet;

    fn snippet(name: &str, value: &str, keyword: Option<&str>) -> TextSnippet {
        TextSnippet {
            icon: None,
            name: name.to_string(),
            value: value.to_string(),
            keyword: keyword.map(str::to_string),
        }
    }

    #[test]
    fn test_filter_snippets_empty_query_returns_all() {
        let snippets = vec![
            snippet("Alpha", "a", None),
            snippet("Beta", "b", None),
            snippet("Gamma", "c", None),
        ];

        let result = filter_snippets(&snippets, "", &[]);
        assert_eq!(result, vec![0, 1, 2]);
    }

    #[test]
    fn test_filter_snippets_partial_query() {
        let snippets = vec![
            snippet("Personal Email", "user@example.com", None),
            snippet("Work Email", "user@company.com", None),
            snippet("Phone Number", "+1234567890", None),
        ];

        let result = filter_snippets(&snippets, "work", &[]);
        assert!(!result.is_empty());
        assert!(result.contains(&1));
    }

    #[test]
    fn test_filter_snippets_no_match() {
        let snippets = vec![snippet("Alpha", "a", None), snippet("Beta", "b", None)];

        let result = filter_snippets(&snippets, "zzzzz", &[]);
        assert!(result.is_empty());
    }

    #[test]
    fn test_filter_snippets_ranks_keyword_then_name_then_content() {
        let snippets = vec![
            snippet("Mentions sig in body", "my sig here", None),
            snippet("Signature", "Best regards", None),
            snippet("Prefix", "x", Some("sigmund")),
            snippet("Exact", "y", Some("SIG")),
        ];

        assert_eq!(filter_snippets(&snippets, "sig", &[]), vec![3, 2, 1, 0]);
    }

    #[test]
    fn test_filter_snippets_matches_content_and_keeps_ties_stable() {
        let snippets = vec![
            snippet("One", "kubectl get pods", None),
            snippet("Two", "unrelated", None),
            snippet("Three", "kubectl get pods", None),
        ];

        assert_eq!(filter_snippets(&snippets, "kubectl", &[]), vec![0, 2]);
    }

    #[test]
    fn test_filter_snippets_orders_by_usage_except_bang_queries() {
        let snippets = vec![
            snippet("Mail one", "a", Some("!mail1")),
            snippet("Mail two", "b", Some("!mail2")),
            snippet("Other", "c", None),
        ];
        let usage = [1, 5, 9];

        assert_eq!(filter_snippets(&snippets, "", &usage), vec![2, 1, 0]);
        // Usage breaks the tie between equal fuzzy scores.
        assert_eq!(filter_snippets(&snippets, "mail", &usage), vec![1, 0]);
        // Match quality still wins over usage.
        assert_eq!(filter_snippets(&snippets, "mail one", &usage)[0], 0);
        // A `!` query keeps the keyword ranking and source order.
        assert_eq!(filter_snippets(&snippets, "!mail", &usage), vec![0, 1]);
    }

    #[test]
    fn test_parse_snippet_file() {
        let parsed = parse_snippet_file("Greeting\n---\nHello\nthere").unwrap();
        assert_eq!(parsed, snippet("Greeting", "Hello\nthere", None));

        let parsed = parse_snippet_file("Signature\nkeyword: !sig\n---\n-- me").unwrap();
        assert_eq!(parsed, snippet("Signature", "-- me", Some("!sig")));

        for headers in ["icon: ✍️\nkeyword: !sig", "keyword: !sig\nicon: ✍️"] {
            let parsed =
                parse_snippet_file(&format!("Signature\n{headers}\n---\nicon: body")).unwrap();
            assert_eq!(parsed.name, "Signature");
            assert_eq!(parsed.keyword.as_deref(), Some("!sig"));
            assert_eq!(parsed.icon.as_deref(), Some("✍️"));
            assert_eq!(parsed.value, "icon: body");
        }
        assert_eq!(
            parse_snippet_file("Name\nicon: \n---\nbody").unwrap().icon,
            None
        );

        assert!(parse_snippet_file("Name\nnot a header\n---\nvalue").is_none());
        assert!(parse_snippet_file("Name\nvalue without separator").is_none());
        assert!(parse_snippet_file("Name\n---\n").is_none());
    }

    #[test]
    fn test_command_icons_use_alfred_format() {
        let response: super::SnippetCommandResponse = serde_json::from_str(
            r#"{"items":[{"title":"Mail","arg":"body","icon":{"path":"📨"}},{"title":"Plain"}]}"#,
        )
        .unwrap();
        assert_eq!(
            response.items[0].icon.as_ref().unwrap().path.as_deref(),
            Some("📨")
        );
        assert!(response.items[1].icon.is_none());
    }

    #[test]
    fn test_truncate_preview_keeps_multibyte_characters_whole() {
        let value = format!("{}é and more text", "a".repeat(79));
        // The old byte slice at 80 would split the two-byte 'é' and panic.
        assert_eq!(truncate_preview(&value), format!("{}é...", "a".repeat(79)));
        assert_eq!(truncate_preview("short"), "short");
        assert_eq!(
            truncate_preview("# Title\n\nline one\nline two\r\nline three"),
            "# Title line one line two line three"
        );
    }

    #[test]
    fn test_compute_previews_uses_clipboard_snapshot() {
        let snippets = vec![
            snippet("Clip", "<{clipboard}>", None),
            snippet("Shell", "{shell:date}", None),
            snippet("Broken", "{date:%Q}", None),
            snippet("History", "{clipboard:1|uppercase}", None),
        ];

        let previews = compute_previews(&snippets, true, &ClipboardData::default());
        assert_eq!(
            previews,
            vec![None, Some("{shell:date}".to_string()), None, None]
        );

        let mut data = ClipboardData {
            current: Some(Ok("copied".into())),
            ..Default::default()
        };
        data.history.insert(1, Ok("older".into()));
        let previews = compute_previews(&snippets, true, &data);
        assert_eq!(previews[0].as_deref(), Some("<copied>"));
        assert_eq!(previews[3].as_deref(), Some("OLDER"));

        let needs = missing_clipboard(&snippets, &ClipboardData::default());
        assert!(needs.current);
        assert_eq!(needs.history.iter().copied().collect::<Vec<_>>(), vec![1]);
        assert!(missing_clipboard(&snippets, &data).is_empty());
    }

    fn temp_markdown(name: &str) -> (TestDir, std::path::PathBuf, String) {
        let dir = TestDir::new(name);
        let path = dir.path().join("snippets.md");
        let path_str = path.to_string_lossy().to_string();
        (dir, path, path_str)
    }

    #[test]
    fn test_parse_markdown_snippets_reads_sections_keywords_and_bodies() {
        let md = "# Snippets\n\nintro text is ignored\n\n## Greeting\n\nHello,\nworld\n\n\
                  ## Signature\nkeyword: !sig\n\nBest regards\n\n## Empty\n\n## Last\n\nend\n";
        let parsed = parse_markdown_snippets(md);
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0].name, "Greeting");
        assert_eq!(parsed[0].value, "Hello,\nworld");
        assert_eq!(parsed[0].keyword, None);
        assert_eq!(parsed[1].keyword.as_deref(), Some("!sig"));
        assert_eq!(parsed[1].value, "Best regards");
        assert_eq!(parsed[2].name, "Last");
    }

    #[test]
    fn test_markdown_body_round_trips_headings_and_keyword_lookalikes() {
        let tricky = "## not a section\n\\## escaped\nkeyword: looks like a header\n\n- kept";
        let original = snippet("Tricky", tricky, Some("!t"));
        let rendered = render_markdown_snippet(&original);
        let parsed = parse_markdown_snippets(&rendered);
        assert_eq!(parsed, vec![original]);
    }

    #[test]
    fn test_markdown_body_without_keyword_round_trips_a_keyword_first_line() {
        let original = snippet("Header", "keyword: body text", None);
        let parsed = parse_markdown_snippets(&render_markdown_snippet(&original));
        assert_eq!(parsed, vec![original]);
    }

    #[test]
    fn test_append_markdown_snippet_creates_the_file_and_adds_sections() {
        let (_dir, path, path_str) = temp_markdown("md-append");
        append_markdown_snippet(&path_str, &snippet("First", "one", None)).unwrap();
        append_markdown_snippet(&path_str, &snippet("Second", "two", Some("!2"))).unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            contents,
            "## First\n\none\n\n## Second\nkeyword: !2\n\ntwo\n"
        );
        let read = read_snippet_markdown(&path_str).unwrap();
        assert_eq!(
            read.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            ["First", "Second"]
        );
        assert_eq!(
            std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
            1
        );
    }

    #[test]
    fn test_append_markdown_snippet_refuses_a_duplicate_name() {
        let (_dir, _path, path_str) = temp_markdown("md-duplicate");
        append_markdown_snippet(&path_str, &snippet("Same", "one", None)).unwrap();
        let before = std::fs::read_to_string(&path_str).unwrap();
        let error = append_markdown_snippet(&path_str, &snippet("Same", "two", None)).unwrap_err();
        assert!(error.contains("already exists"), "{error}");
        assert_eq!(std::fs::read_to_string(&path_str).unwrap(), before);
    }

    #[test]
    fn test_append_markdown_snippet_separates_from_a_file_without_newline() {
        let (_dir, path, path_str) = temp_markdown("md-no-newline");
        std::fs::write(&path, "## Hand written\n\nbody").unwrap();
        append_markdown_snippet(&path_str, &snippet("Added", "more", None)).unwrap();
        let parsed = read_snippet_markdown(&path_str).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].value, "body");
        assert_eq!(parsed[1].value, "more");
    }

    #[test]
    fn test_read_missing_markdown_file_is_an_empty_source() {
        let (_dir, _path, path_str) = temp_markdown("md-missing");
        assert!(read_snippet_markdown(&path_str).unwrap().is_empty());
    }

    fn temp_dir_path(name: &str) -> (TestDir, String) {
        let dir = TestDir::new(name);
        let path = dir.path().join("snippets").to_string_lossy().to_string();
        (dir, path)
    }

    #[test]
    fn test_save_snippet_to_dir_round_trips_through_the_directory_reader() {
        let (_guard, dir) = temp_dir_path("dir-roundtrip");
        let tricky = "first\n---\nkeyword: not a header\n## heading\n\n  indented\nlast";
        save_snippet_to_dir(&dir, &snippet("My Sig: v2!", tricky, Some("!sig"))).unwrap();
        save_snippet_to_dir(&dir, &snippet("plain", "text", None)).unwrap();
        let read = super::read_snippet_dir(&dir).unwrap();
        assert_eq!(read.len(), 2);
        let sig = read.iter().find(|s| s.name == "My Sig: v2!").unwrap();
        assert_eq!(sig.value, tricky);
        assert_eq!(sig.keyword.as_deref(), Some("!sig"));
        assert!(std::path::Path::new(&dir)
            .join("my-sig-v2.snippet")
            .is_file());
        assert!(std::fs::read_dir(&dir)
            .unwrap()
            .all(|e| e.unwrap().path().extension().unwrap() == "snippet"));
    }

    #[test]
    fn test_save_snippet_to_dir_refuses_duplicates_and_keeps_colliding_file_names() {
        let (_guard, dir) = temp_dir_path("dir-collide");
        save_snippet_to_dir(&dir, &snippet("A b", "one", None)).unwrap();
        let error = save_snippet_to_dir(&dir, &snippet("A b", "two", None)).unwrap_err();
        assert!(error.contains("already exists"), "{error}");
        // A different name with the same file stem gets its own file.
        save_snippet_to_dir(&dir, &snippet("a-b", "three", None)).unwrap();
        let read = super::read_snippet_dir(&dir).unwrap();
        assert_eq!(read.len(), 2);
        assert!(std::path::Path::new(&dir).join("a-b-2.snippet").is_file());
        let value = |name: &str| read.iter().find(|s| s.name == name).unwrap().value.clone();
        assert_eq!(value("A b"), "one");
        assert_eq!(value("a-b"), "three");
    }

    #[test]
    fn test_save_snippet_to_dir_refuses_bad_names_and_empty_text() {
        let (_guard, dir) = temp_dir_path("dir-bad");
        assert!(save_snippet_to_dir(&dir, &snippet("  ", "x", None)).is_err());
        assert!(save_snippet_to_dir(&dir, &snippet("two\nlines", "x", None)).is_err());
        assert!(save_snippet_to_dir(&dir, &snippet("n", " \n ", None)).is_err());
        // A name that is all punctuation still gets a usable file name.
        save_snippet_to_dir(&dir, &snippet("???", "x", None)).unwrap();
        assert!(std::path::Path::new(&dir).join("snippet.snippet").is_file());
        // A path separator in the name cannot leave the directory.
        save_snippet_to_dir(&dir, &snippet("../../evil", "x", None)).unwrap();
        assert!(std::path::Path::new(&dir).join("evil.snippet").is_file());
    }

    fn mode_of(path: impl AsRef<std::path::Path>) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn test_append_markdown_snippet_keeps_the_content_and_mode_of_an_existing_file() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, path, path_str) = temp_markdown("md-mode");
        std::fs::write(&path, "## Old\n\nbody\n").unwrap();
        for mode in [0o600, 0o644] {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            let name = format!("Added {mode:o}");
            append_markdown_snippet(&path_str, &snippet(&name, "more", None)).unwrap();
            assert_eq!(mode_of(&path), mode);
        }
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(
            contents.starts_with("## Old\n\nbody\n\n## Added 600\n"),
            "{contents}"
        );
        assert_eq!(read_snippet_markdown(&path_str).unwrap().len(), 3);
    }

    #[test]
    fn test_new_snippet_files_and_folders_are_private() {
        let (_dir, path, path_str) = temp_markdown("md-private");
        let nested = path.parent().unwrap().join("a/b/snippets.md");
        append_markdown_snippet(&nested.to_string_lossy(), &snippet("N", "x", None)).unwrap();
        assert_eq!(mode_of(&nested), 0o600);
        assert_eq!(mode_of(nested.parent().unwrap()), 0o700);
        append_markdown_snippet(&path_str, &snippet("M", "x", None)).unwrap();
        assert_eq!(mode_of(&path), 0o600);

        let (_guard, dir) = temp_dir_path("dir-private");
        save_snippet_to_dir(&dir, &snippet("Private", "x", None)).unwrap();
        assert_eq!(mode_of(&dir), 0o700);
        assert_eq!(
            mode_of(std::path::Path::new(&dir).join("private.snippet")),
            0o600
        );
    }

    #[test]
    fn test_save_snippet_to_dir_ignores_a_planted_temporary_symlink() {
        let (guard, dir) = temp_dir_path("dir-symlink");
        std::fs::create_dir_all(&dir).unwrap();
        let decoy = guard.path().join("decoy");
        std::fs::write(&decoy, "untouched").unwrap();
        for name in ["target.snippet.tmp", "target.tmp", "target.snippet"] {
            std::os::unix::fs::symlink(&decoy, std::path::Path::new(&dir).join(name)).unwrap();
        }
        save_snippet_to_dir(&dir, &snippet("Target", "text", None)).unwrap();
        assert_eq!(std::fs::read_to_string(&decoy).unwrap(), "untouched");
        let saved = super::read_snippet_dir(&dir).unwrap();
        assert!(saved
            .iter()
            .any(|s| s.name == "Target" && s.value == "text"));
    }

    #[test]
    fn test_save_snippet_to_dir_leaves_no_temporary_file() {
        let (_guard, dir) = temp_dir_path("dir-clean");
        save_snippet_to_dir(&dir, &snippet("One", "x", None)).unwrap();
        let _ = save_snippet_to_dir(&dir, &snippet("One", "y", None));
        let names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(names, ["one.snippet"]);
    }

    #[test]
    fn test_same_stem_saves_never_overwrite_each_other() {
        let (_guard, dir) = temp_dir_path("dir-stem");
        let names = ["A b", "a-b", "A_b", "a b!"];
        for (index, name) in names.iter().enumerate() {
            save_snippet_to_dir(&dir, &snippet(name, &format!("value {index}"), None)).unwrap();
        }
        let read = super::read_snippet_dir(&dir).unwrap();
        assert_eq!(read.len(), names.len());
        for (index, name) in names.iter().enumerate() {
            let saved = read.iter().find(|s| s.name == *name).unwrap();
            assert_eq!(saved.value, format!("value {index}"));
        }
    }

    #[test]
    fn test_save_snippet_escapes_placeholders_only_when_the_source_expands_them() {
        let value = "run {shell:touch x} on {date}";
        let (_guard, dir) = temp_dir_path("dir-escape");
        let source = |placeholders: bool, directory: String| crate::TextSnippetSourceConfig {
            directory: Some(directory),
            placeholders,
            ..Default::default()
        };
        let on = format!("{dir}/on");
        save_snippet(&source(true, on.clone()), &snippet("Run", value, None)).unwrap();
        let stored = std::fs::read_to_string(format!("{on}/run.snippet")).unwrap();
        assert!(
            stored.contains("run \\{shell:touch x} on \\{date}"),
            "{stored}"
        );

        let off = format!("{dir}/off");
        save_snippet(&source(false, off.clone()), &snippet("Run", value, None)).unwrap();
        let stored = std::fs::read_to_string(format!("{off}/run.snippet")).unwrap();
        assert!(stored.contains(value) && !stored.contains('\\'), "{stored}");

        let markdown = format!("{dir}/snippets.md");
        let config = crate::TextSnippetSourceConfig {
            markdown: Some(markdown.clone()),
            placeholders: true,
            ..Default::default()
        };
        save_snippet(&config, &snippet("Run", value, None)).unwrap();
        let stored = std::fs::read_to_string(markdown).unwrap();
        assert!(
            stored.contains("run \\{shell:touch x} on \\{date}"),
            "{stored}"
        );
    }

    #[test]
    fn test_snippet_save_path_follows_the_source_that_is_read() {
        let config = |f: &dyn Fn(&mut crate::TextSnippetSourceConfig)| {
            let mut c = crate::TextSnippetSourceConfig::default();
            f(&mut c);
            c
        };
        let dir = config(&|c| c.directory = Some("/d".into()));
        assert_eq!(snippet_save_path(&dir), Some("/d"));
        let md = config(&|c| c.markdown = Some("/m.md".into()));
        assert_eq!(snippet_save_path(&md), Some("/m.md"));
        // The directory is read before the markdown file, so it is the target.
        let both = config(&|c| {
            c.directory = Some("/d".into());
            c.markdown = Some("/m.md".into());
        });
        assert_eq!(snippet_save_path(&both), Some("/d"));
        // Inline and `file` sources are read first, so a save would not show.
        let inline = config(&|c| {
            c.snippets = Some(Vec::new());
            c.directory = Some("/d".into());
        });
        assert_eq!(snippet_save_path(&inline), None);
        assert_eq!(snippet_save_path(&config(&|_| {})), None);
    }
}
