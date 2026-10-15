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
            .find_map(|icon| {
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
                    return Some(SnippetIcon::Glyph(icon.clone()));
                }
                None
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
        async move {
            crate::debug_log!("snippets: executing command={command:?} args={args:?}");
            let output = Command::new(&command)
                .args(&args)
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
        },
        move |result| Message::TextSnippetCommandResult(generation, result),
    )
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

/// Rank snippets for `query`: exact keyword, keyword prefix, fuzzy name, then
/// fuzzy content. Ties keep the original order.
pub(super) fn filter_snippets(snippets: &[TextSnippet], query: &str) -> Vec<usize> {
    if query.is_empty() {
        return (0..snippets.len()).collect();
    }

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
    ranked.sort_by_key(|&(tier, score, _)| (tier, std::cmp::Reverse(score)));
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

#[cfg(test)]
mod tests {
    use super::{
        compute_previews, filter_snippets, missing_clipboard, parse_snippet_file, truncate_preview,
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

        let result = filter_snippets(&snippets, "");
        assert_eq!(result, vec![0, 1, 2]);
    }

    #[test]
    fn test_filter_snippets_partial_query() {
        let snippets = vec![
            snippet("Personal Email", "user@example.com", None),
            snippet("Work Email", "user@company.com", None),
            snippet("Phone Number", "+1234567890", None),
        ];

        let result = filter_snippets(&snippets, "work");
        assert!(!result.is_empty());
        assert!(result.contains(&1));
    }

    #[test]
    fn test_filter_snippets_no_match() {
        let snippets = vec![snippet("Alpha", "a", None), snippet("Beta", "b", None)];

        let result = filter_snippets(&snippets, "zzzzz");
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

        assert_eq!(filter_snippets(&snippets, "sig"), vec![3, 2, 1, 0]);
    }

    #[test]
    fn test_filter_snippets_matches_content_and_keeps_ties_stable() {
        let snippets = vec![
            snippet("One", "kubectl get pods", None),
            snippet("Two", "unrelated", None),
            snippet("Three", "kubectl get pods", None),
        ];

        assert_eq!(filter_snippets(&snippets, "kubectl"), vec![0, 2]);
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
}
