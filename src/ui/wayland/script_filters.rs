use std::borrow::Cow;
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use iced::Task;
use serde::Deserialize;

use super::cache::{self, SnapshotTarget};
use super::state::{Message, ScriptFilterResult};
use super::types::{
    LazyChildrenKey, PreparedPreview, RawPreviewSpec, ScriptFilterItem, ScriptFilterPreviewError,
    ScriptFilterView,
};

/// Longest a `RAFFI_PREVIEW=1` run may take before it is killed.
const PREVIEW_TIMEOUT: Duration = Duration::from_secs(3);
/// Longest a `RAFFI_CHILDREN=1` run may take before it is killed.
const CHILDREN_TIMEOUT: Duration = Duration::from_secs(10);
/// Largest list output accepted from a time-limited run.
const MAX_LIST_OUTPUT: usize = 64 * 1024 * 1024;
/// Largest preview output accepted, enough for a base64 screenshot.
const MAX_PREVIEW_OUTPUT: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Deserialize)]
struct ScriptFilterResponse {
    items: Vec<ScriptFilterItem>,
    #[serde(default)]
    views: Vec<ScriptFilterView>,
    #[serde(default)]
    view: Option<String>,
    /// Kept raw so that a malformed hint never fails the whole list.
    #[serde(default)]
    cache: Option<Box<serde_json::value::RawValue>>,
    /// `"usage"` asks for picked rows first; anything else, or nothing,
    /// keeps the order the script prints. Kept raw like `cache`.
    #[serde(default)]
    sort: Option<Box<serde_json::value::RawValue>>,
}

/// What a script said about caching its output.
#[derive(Default)]
struct CacheHint {
    seconds: Option<u64>,
    warm: Vec<String>,
}

/// A `cache` hint: `{"seconds": N, "warm": ["query", ...]}`, or a bare `N`.
/// Anything that does not fit is left out, so an odd hint never costs more
/// than itself.
fn parse_cache_hint(raw: &serde_json::value::RawValue) -> CacheHint {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(raw.get()) else {
        return CacheHint::default();
    };
    match value {
        serde_json::Value::Object(fields) => CacheHint {
            seconds: fields.get("seconds").and_then(serde_json::Value::as_u64),
            warm: fields
                .get("warm")
                .and_then(serde_json::Value::as_array)
                .map(|queries| {
                    queries
                        .iter()
                        .filter_map(|query| query.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
        },
        other => CacheHint {
            seconds: other.as_u64(),
            warm: Vec::new(),
        },
    }
}

pub(super) fn parse_script_filter_output(
    stdout: &str,
    default_icon: Option<String>,
) -> Result<ScriptFilterResult, String> {
    serde_json::from_str::<ScriptFilterResponse>(stdout)
        .map(|response| {
            let hint = response
                .cache
                .as_deref()
                .map(parse_cache_hint)
                .unwrap_or_default();
            ScriptFilterResult {
                items: response.items,
                default_icon,
                views: response.views,
                view: response.view,
                cache_hint: hint.seconds,
                not_stored: hint.seconds == Some(0),
                cache_warm: hint.warm,
                sort_by_usage: response.sort.as_deref().is_some_and(|sort| {
                    serde_json::from_str::<String>(sort.get()).is_ok_and(|sort| sort == "usage")
                }),
            }
        })
        .map_err(|error| format!("Invalid JSON: {}", error))
}

/// `config` as run for `view`: its `variables` and `RAFFI_VIEW` join the
/// environment. No view keeps the config as written, the script's default.
pub(super) fn view_config<'a>(
    config: &'a crate::ScriptFilterConfig,
    view: Option<&ScriptFilterView>,
) -> Cow<'a, crate::ScriptFilterConfig> {
    let Some(view) = view else {
        return Cow::Borrowed(config);
    };
    let mut config = config.clone();
    config.env.extend(view.variables.clone());
    config.env.insert("RAFFI_VIEW".into(), view.id.clone());
    Cow::Owned(config)
}

pub(super) fn execute_script_filter(
    command: String,
    args: Vec<String>,
    env: std::collections::HashMap<String, String>,
    query: String,
    generation: u64,
    default_icon: Option<String>,
    cache: Option<SnapshotTarget>,
) -> Task<Message> {
    Task::perform(
        async move { run_script_filter(&command, &args, &env, &query, default_icon, cache, None) },
        move |result| Message::ScriptFilterResult(generation, result),
    )
}

/// Put a run's output in its snapshot. The file is serialized and synced
/// first, then handed to the target, which renames it only if no later run has
/// taken the list over in the meantime.
fn publish_snapshot(target: &SnapshotTarget, payload: &str, hint_secs: Option<u64>) {
    let prepared = match cache::prepare_snapshot_path(
        &target.path,
        payload,
        target.ttl,
        target.keep,
        hint_secs,
    ) {
        Ok(prepared) => prepared,
        Err(error) => {
            crate::debug_log!("script_filter: cannot write cache: {error}");
            return;
        }
    };
    if let Err(error) = target.publish(prepared) {
        crate::debug_log!("script_filter: cannot write cache: {error}");
    }
}

/// Run a script filter for `query` and parse its items. Blocking. With a
/// `timeout` the script runs in its own process group, which is killed when
/// the time is up.
pub(super) fn run_script_filter(
    command: &str,
    args: &[String],
    env: &HashMap<String, String>,
    query: &str,
    default_icon: Option<String>,
    cache: Option<SnapshotTarget>,
    timeout: Option<Duration>,
) -> Result<ScriptFilterResult, String> {
    crate::debug_log!("script_filter: executing command={command:?} args={args:?} query={query:?}");
    let mut process = crate::process::command(command);
    process.args(args).envs(env).arg(query);
    let output = match timeout {
        Some(timeout) => super::actions::run_bounded_status(process, timeout, MAX_LIST_OUTPUT)
            .map_err(|error| format!("Failed to execute: {error}")),
        None => process
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .map(|output| (output.status, output.stdout))
            .map_err(|error| format!("Failed to execute: {error}")),
    };

    match output {
        Ok((status, stdout)) if status.success() => {
            let stdout = String::from_utf8_lossy(&stdout);
            match parse_script_filter_output(&stdout, default_icon) {
                Ok(result) => {
                    if let Some(target) = cache.filter(|_| !result.not_stored) {
                        publish_snapshot(&target, &stdout, result.cache_hint);
                    }
                    crate::debug_log!(
                        "script_filter: got {} items from {command:?}",
                        result.items.len()
                    );
                    Ok(result)
                }
                Err(error) => {
                    eprintln!("Script filter: invalid JSON from {}: {}", command, error);
                    crate::debug_log!("script_filter: invalid JSON from {command:?}: {error}");
                    Err(error)
                }
            }
        }
        Ok((status, _)) => {
            eprintln!("Script filter: {} exited with status {}", command, status);
            crate::debug_log!("script_filter: {command:?} exited with status {status}");
            Err(format!("Script exited with status {status}"))
        }
        Err(error) => {
            eprintln!("Script filter: failed to execute {}: {}", command, error);
            crate::debug_log!("script_filter: failed to execute {command:?}: {error}");
            Err(error)
        }
    }
}

/// Parse the output of a `RAFFI_PREVIEW=1` run. `Ok(None)` means the script
/// does not speak the preview protocol: the output is not a JSON object or is
/// an item list. Any other object is a preview, `{}` an empty one.
pub(super) fn parse_preview_response(
    stdout: &[u8],
) -> Result<Option<PreparedPreview>, ScriptFilterPreviewError> {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(stdout) else {
        return Ok(None);
    };
    if !value
        .as_object()
        .is_some_and(|object| !object.contains_key("items"))
    {
        return Ok(None);
    }
    let spec: RawPreviewSpec = serde_json::from_value(value)
        .map_err(|error| ScriptFilterPreviewError::Invalid(format!("Invalid preview: {error}")))?;
    Ok(Some(spec.into()))
}

/// Everything needed to run one dynamic preview of a Script Filter item.
#[derive(Debug, Clone)]
pub(super) struct PreviewRequest {
    pub command: String,
    pub args: Vec<String>,
    pub query: String,
    pub env: HashMap<String, String>,
    pub cache: Option<SnapshotTarget>,
}

impl PreviewRequest {
    /// Environment for a preview run: the config `env`, then the item
    /// `variables`, then the `RAFFI_*` protocol variables.
    pub fn new(
        config: &crate::ScriptFilterConfig,
        query: &str,
        item: &ScriptFilterItem,
        pane_size: (u32, u32),
    ) -> Self {
        let mut env = config.env.clone();
        env.extend(item.variables.clone());
        let value = item.arg.clone().unwrap_or_else(|| item.title.clone());
        env.insert("RAFFI_PREVIEW".into(), "1".into());
        env.insert("RAFFI_VALUE".into(), value);
        env.insert("RAFFI_TITLE".into(), super::ansi::strip_ansi(&item.title));
        env.insert("RAFFI_QUERY".into(), query.to_string());
        env.insert("RAFFI_PREVIEW_WIDTH".into(), pane_size.0.to_string());
        env.insert("RAFFI_PREVIEW_HEIGHT".into(), pane_size.1.to_string());
        Self {
            command: config.command.clone(),
            args: config.args.clone(),
            query: query.to_string(),
            env,
            cache: None,
        }
    }

    pub(super) fn run(self) -> Result<Option<PreparedPreview>, ScriptFilterPreviewError> {
        crate::debug_log!(
            "script_filter: preview command={:?} value={:?}",
            self.command,
            self.env.get("RAFFI_VALUE")
        );
        let mut command = crate::process::command(&self.command);
        command.args(&self.args).arg(&self.query).envs(&self.env);
        let output = super::actions::run_bounded(command, PREVIEW_TIMEOUT, MAX_PREVIEW_OUTPUT)
            .map_err(ScriptFilterPreviewError::Run)?;
        let preview = parse_preview_response(&output)?;
        if preview.is_some() {
            if let Some(target) = self.cache {
                let payload = String::from_utf8_lossy(&output);
                if let Err(error) = cache::write_snapshot_path(
                    &target.path,
                    &payload,
                    target.ttl,
                    target.keep,
                    None,
                ) {
                    crate::debug_log!("script_filter: cannot write preview cache: {error}");
                }
            }
        }
        Ok(preview)
    }
}

/// Run the preview command on a worker thread.
pub(super) fn execute_script_filter_preview(
    request: PreviewRequest,
    generation: u64,
    config_index: usize,
    index: usize,
) -> Task<Message> {
    Task::perform(
        async move {
            let (sender, receiver) = iced::futures::channel::oneshot::channel();
            std::thread::spawn(move || {
                let _ = sender.send(request.run());
            });
            receiver.await.unwrap_or_else(|_| {
                Err(ScriptFilterPreviewError::Run(
                    "preview worker stopped".into(),
                ))
            })
        },
        move |result| Message::ScriptFilterPreview(generation, config_index, index, result),
    )
}

/// Everything needed to fetch a row's children lazily with `RAFFI_CHILDREN=1`.
#[derive(Debug, Clone)]
pub(super) struct ChildrenRequest {
    pub command: String,
    pub args: Vec<String>,
    pub query: String,
    pub env: HashMap<String, String>,
    pub default_icon: Option<String>,
}

impl ChildrenRequest {
    /// Environment for a lazy children run: the config `env`, then the item
    /// `variables`, then the `RAFFI_*` protocol variables.
    pub fn new(
        config: &crate::ScriptFilterConfig,
        query: &str,
        item: &ScriptFilterItem,
        force_refresh: bool,
    ) -> Self {
        let mut env = config.env.clone();
        env.extend(item.variables.clone());
        let value = item.arg.clone().unwrap_or_else(|| item.title.clone());
        env.insert("RAFFI_CHILDREN".into(), "1".into());
        env.insert("RAFFI_VALUE".into(), value);
        env.insert("RAFFI_TITLE".into(), super::ansi::strip_ansi(&item.title));
        env.insert("RAFFI_QUERY".into(), query.to_string());
        if force_refresh {
            env.insert("RAFFI_REFRESH".into(), "1".into());
        }
        Self {
            command: config.command.clone(),
            args: config.args.clone(),
            query: query.to_string(),
            env,
            default_icon: config.icon.clone(),
        }
    }

    pub(super) fn run(self) -> Result<ScriptFilterResult, String> {
        crate::debug_log!(
            "script_filter: children command={:?} value={:?}",
            self.command,
            self.env.get("RAFFI_VALUE")
        );
        run_script_filter(
            &self.command,
            &self.args,
            &self.env,
            &self.query,
            self.default_icon,
            None,
            Some(CHILDREN_TIMEOUT),
        )
    }
}

/// Run the lazy children command on a worker thread.
pub(super) fn execute_script_filter_children(
    request: ChildrenRequest,
    generation: u64,
    cache_key: LazyChildrenKey,
) -> Task<Message> {
    Task::perform(
        async move {
            let (sender, receiver) = iced::futures::channel::oneshot::channel();
            std::thread::spawn(move || {
                let _ = sender.send(request.run());
            });
            receiver
                .await
                .unwrap_or_else(|_| Err("children worker stopped".into()))
        },
        move |result| Message::ScriptFilterChildrenResult(generation, cache_key, result),
    )
}

/// Sleep for `delay`, waking every 100 ms to stop early once `live` no longer
/// holds `generation`.
fn sleep_unless_superseded(delay: Duration, generation: u64, live: &AtomicU64) {
    const POLL: Duration = Duration::from_millis(100);
    let deadline = std::time::Instant::now() + delay;
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() || live.load(Ordering::Relaxed) != generation {
            return;
        }
        std::thread::sleep(left.min(POLL));
    }
}

/// Fire `ScriptFilterPreviewDue` after `delay`, or sooner once `live` moves
/// past `generation` (the message is then ignored as stale).
pub(super) fn schedule_script_filter_preview(
    delay: Duration,
    generation: u64,
    live: Arc<AtomicU64>,
    index: usize,
) -> Task<Message> {
    Task::perform(
        async move {
            let (sender, receiver) = iced::futures::channel::oneshot::channel();
            std::thread::spawn(move || {
                sleep_unless_superseded(delay, generation, &live);
                let _ = sender.send(());
            });
            let _ = receiver.await;
        },
        move |()| Message::ScriptFilterPreviewDue(generation, index),
    )
}

#[cfg(test)]
mod tests {
    use super::super::types::{
        PreparedPreview, PreparedPreviewBody, ScriptFilterPreview, ScriptFilterPreviewError,
    };
    use super::{parse_preview_response, parse_script_filter_output, sleep_unless_superseded};
    use std::sync::atomic::AtomicU64;
    use std::time::{Duration, Instant};

    #[test]
    fn test_script_filter_validity_defaults_true_and_preserves_false() {
        let result = parse_script_filter_output(
            r#"{"items":[{"title":"Agent"},{"title":"Working","valid":false}]}"#,
            None,
        )
        .unwrap();
        assert!(result.items[0].valid);
        assert!(!result.items[1].valid);
    }

    #[test]
    fn test_view_runs_get_view_env_and_title_defaults_to_id() {
        let output = r#"{"items":[],"view":"b","views":[
            {"id":"a"},{"id":"b","title":"Bee","variables":{"MODE":"bee","KEEP":"view"}}]}"#;
        let result = parse_script_filter_output(output, None).unwrap();
        assert_eq!(result.view.as_deref(), Some("b"));
        assert_eq!(result.views[0].title, "a");
        assert_eq!(result.views[1].title, "Bee");

        let config = crate::ScriptFilterConfig {
            command: "sh".into(),
            args: vec![
                "-c".into(),
                r#"printf '{"items":[{"title":"%s %s %s"}]}' "$RAFFI_VIEW" "$MODE" "$KEEP""#.into(),
            ],
            env: [("KEEP".into(), "config".into())].into(),
            ..Default::default()
        };
        assert!(matches!(
            super::view_config(&config, None),
            std::borrow::Cow::Borrowed(_)
        ));
        let config = super::view_config(&config, Some(&result.views[1]));
        let run = super::run_script_filter(
            &config.command,
            &config.args,
            &config.env,
            "",
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(run.items[0].title, "b bee view");
    }

    #[test]
    fn test_preview_runs_carry_the_view_env() {
        let output = r#"{"items":[],"views":[{"id":"review","variables":{"MODE":"view"}}]}"#;
        let result = parse_script_filter_output(output, None).unwrap();
        let config = crate::ScriptFilterConfig {
            command: "sh".into(),
            args: vec!["-c".into(), "true".into()],
            env: [
                ("MODE".into(), "config".into()),
                ("KEEP".into(), "kept".into()),
            ]
            .into(),
            ..Default::default()
        };
        let config = super::view_config(&config, Some(&result.views[0]));
        let item = super::ScriptFilterItem {
            actions: Vec::new(),
            time: None,
            children: None,
            children_lazy: false,
            title: "Agent".into(),
            valid: true,
            subtitle: None,
            arg: Some("w1:p1".into()),
            indent: 0,
            icon: None,
            preview: None,
            variables: Default::default(),
            quicklookurl: None,
            text: None,
        };
        let request = super::PreviewRequest::new(&config, "query", &item, (400, 600));
        assert_eq!(
            request.env.get("RAFFI_VIEW").map(String::as_str),
            Some("review")
        );
        assert_eq!(request.env.get("MODE").map(String::as_str), Some("view"));
        assert_eq!(request.env.get("KEEP").map(String::as_str), Some("kept"));
    }

    #[test]
    fn test_superseded_timer_wakes_early() {
        let live = AtomicU64::new(2);
        let start = Instant::now();
        sleep_unless_superseded(Duration::from_secs(60), 1, &live);
        assert!(start.elapsed() < Duration::from_secs(1));

        let start = Instant::now();
        sleep_unless_superseded(Duration::from_millis(150), 2, &live);
        assert!(start.elapsed() >= Duration::from_millis(150));
    }

    #[test]
    fn test_preview_refresh_is_clamped() {
        let huge = parse_preview_response(br#"{"text":"x","refresh":1e100}"#)
            .unwrap()
            .unwrap();
        assert_eq!(huge.refresh, Some(Duration::from_secs(3600)));
        let tiny = parse_preview_response(br#"{"text":"x","refresh":0.01}"#)
            .unwrap()
            .unwrap();
        assert_eq!(tiny.refresh, Some(Duration::from_millis(500)));
    }

    #[test]
    fn test_preview_explicit_empty_sections_replace_body() {
        let empty = parse_preview_response(br#"{"text":"ignored","sections":[]}"#)
            .unwrap()
            .unwrap();
        assert!(empty.bodies.is_empty());
        assert!(empty.replaces_body);
        let header_only = parse_preview_response(br#"{"title":"t"}"#)
            .unwrap()
            .unwrap();
        assert!(!header_only.replaces_body);
    }

    #[test]
    fn test_parse_script_filter_output_valid_json() {
        let stdout = r#"{"items":[{"title":"One","subtitle":"Sub","arg":"value","icon":{"path":"icon.png"}}]}"#;
        let result = parse_script_filter_output(stdout, Some("default.svg".into()))
            .expect("expected valid script filter json");

        assert_eq!(result.items.len(), 1);
        assert_eq!(result.items[0].title, "One");
        assert_eq!(result.items[0].subtitle.as_deref(), Some("Sub"));
        assert_eq!(result.items[0].arg.as_deref(), Some("value"));
        assert_eq!(
            result.items[0]
                .icon
                .as_ref()
                .and_then(|icon| icon.path.as_deref()),
            Some("icon.png")
        );
        assert_eq!(result.default_icon.as_deref(), Some("default.svg"));
        assert!(result.items[0].preview.is_none());
        assert!(result.items[0].quicklookurl.is_none());
        assert!(result.items[0].text.is_none());
        assert_eq!(result.items[0].indent, 0);
    }

    #[test]
    fn test_parse_script_filter_output_with_actions() {
        let stdout = r#"{"items":[
            {"title":"raffi","actions":[
                {"title":"Pull requests","arg":"https://x/pulls"},
                {"title":"Clone","action":"git clone \"$REPO\"","variables":{"REPO":"x"}}
            ]},
            {"title":"plain"}
        ]}"#;
        let result = parse_script_filter_output(stdout, None).expect("valid json");
        let actions = &result.items[0].actions;
        assert_eq!(actions.len(), 2);
        assert_eq!(actions[0].arg.as_deref(), Some("https://x/pulls"));
        assert!(actions[0].action.is_none());
        assert_eq!(actions[1].action.as_deref(), Some("git clone \"$REPO\""));
        assert_eq!(actions[1].variables["REPO"], "x");
        assert!(result.items[1].actions.is_empty());
    }

    #[test]
    fn test_parse_script_filter_output_rejects_an_action_without_a_title() {
        let stdout = r#"{"items":[{"title":"raffi","actions":[{"arg":"x"}]}]}"#;
        assert!(parse_script_filter_output(stdout, None).is_err());
    }

    #[test]
    fn test_parse_script_filter_output_with_indent_levels() {
        let stdout = r#"{"items":[
            {"title":"workspace","arg":"w1"},
            {"title":"tab","arg":"w1:t1","indent":1},
            {"title":"pane","arg":"w1:p1","indent":2}
        ]}"#;
        let result = parse_script_filter_output(stdout, None).expect("valid tree json");

        assert_eq!(
            result
                .items
                .iter()
                .map(|item| item.indent)
                .collect::<Vec<_>>(),
            [0, 1, 2]
        );
        // Absurd levels are clamped when rendered, not when parsed.
        let stdout = r#"{"items":[{"title":"deep","indent":999}]}"#;
        let result = parse_script_filter_output(stdout, None).expect("valid json");
        assert_eq!(result.items[0].indent, 999);
        assert_eq!(
            super::super::view::indent_left(999),
            super::super::view::indent_left(10)
        );
    }

    #[test]
    fn test_parse_script_filter_output_with_preview_and_alfred_fields() {
        let stdout = r##"{
            "items": [
                {
                    "title": "PR #42",
                    "subtitle": "Add preview pane",
                    "arg": "https://github.com/chmouel/raffi/pull/42",
                    "preview": "Detailed PR body\nLine 2",
                    "quicklookurl": "https://github.com/chmouel/raffi/pull/42",
                    "text": {
                        "copy": "#42",
                        "largetype": "Large type text"
                    }
                }
            ]
        }"##;
        let result = parse_script_filter_output(stdout, None)
            .expect("expected valid script filter json with preview fields");

        assert_eq!(result.items.len(), 1);
        let item = &result.items[0];
        assert_eq!(
            item.preview.as_ref().and_then(ScriptFilterPreview::as_text),
            Some("Detailed PR body\nLine 2")
        );
        assert_eq!(
            item.quicklookurl.as_deref(),
            Some("https://github.com/chmouel/raffi/pull/42")
        );
        let text = item.text.as_ref().expect("expected text object");
        assert_eq!(text.copy.as_deref(), Some("#42"));
        assert_eq!(text.largetype.as_deref(), Some("Large type text"));
    }

    #[test]
    fn test_parse_script_filter_output_with_rich_preview_and_variables() {
        // "aGk=" is base64 for "hi".
        let stdout = r#"{"items":[{
            "title": "Agent",
            "arg": "w1:p1",
            "variables": {"PANE": "w1:p1"},
            "preview": {
                "title": "Agent preview",
                "icon": "utilities-terminal",
                "text": "output",
                "image": {"base64": "aGk="},
                "metadata": [["Status", "working"]],
                "dynamic": false
            }
        }]}"#;
        let result = parse_script_filter_output(stdout, None).expect("valid rich preview json");
        let item = &result.items[0];
        assert_eq!(
            item.variables.get("PANE").map(String::as_str),
            Some("w1:p1")
        );
        let preview = item
            .preview
            .as_ref()
            .and_then(ScriptFilterPreview::as_rich)
            .expect("rich preview");
        assert_eq!(preview.title.as_deref(), Some("Agent preview"));
        assert_eq!(preview.icon.as_deref(), Some("utilities-terminal"));
        assert!(!preview.dynamic);
        assert_eq!(
            preview.metadata,
            vec![("Status".to_string(), "working".to_string())]
        );
        // Image comes before text when no sections are given.
        assert!(matches!(
            preview.bodies.as_slice(),
            [
                PreparedPreviewBody::ImageData(_),
                PreparedPreviewBody::Text { content, .. }
            ] if content == "output"
        ));
    }

    #[test]
    fn test_parse_preview_response_sections_and_refresh() {
        let stdout = br#"{
            "sections": [
                {"type": "text", "heading": "Log", "content": "line"},
                {"type": "image", "path": "/tmp/shot.png"},
                {"type": "image", "base64": "data:image/png;base64,aGk="},
                {"type": "image", "base64": "not base64!"},
                {"type": "file", "path": "~/notes.md"}
            ],
            "text": "ignored when sections are set",
            "refresh": 0.1
        }"#;
        let preview = parse_preview_response(stdout)
            .unwrap()
            .expect("preview response");
        assert_eq!(
            preview.refresh,
            Some(super::super::types::MIN_PREVIEW_REFRESH)
        );
        assert!(preview.dynamic);
        assert_eq!(preview.file_path(), Some("~/notes.md"));
        match preview.bodies.as_slice() {
            [PreparedPreviewBody::Text { heading, content }, PreparedPreviewBody::Image(path), PreparedPreviewBody::ImageData(_), PreparedPreviewBody::Text { content: error, .. }, PreparedPreviewBody::File(_)] =>
            {
                assert_eq!(heading.as_deref(), Some("Log"));
                assert_eq!(content, "line");
                assert_eq!(path, "/tmp/shot.png");
                assert!(error.starts_with("Invalid base64 image"));
            }
            other => panic!("unexpected bodies: {other:?}"),
        }
    }

    #[test]
    fn test_parse_preview_response_rejects_non_preview_output() {
        for stdout in [
            &br#"{"items":[{"title":"x"}]}"#[..],
            b"plain text",
            br#"["text"]"#,
        ] {
            assert_eq!(parse_preview_response(stdout), Ok(None));
        }
        assert!(parse_preview_response(br#"{"text":"ok"}"#)
            .unwrap()
            .is_some());
    }

    #[test]
    fn test_parse_preview_response_any_object_without_items_is_a_preview() {
        let empty = parse_preview_response(b"{}")
            .unwrap()
            .expect("empty preview");
        assert_eq!(
            empty,
            PreparedPreview {
                dynamic: true,
                ..PreparedPreview::default()
            }
        );
        assert!(parse_preview_response(br#"{"unrelated":1}"#)
            .unwrap()
            .is_some());
    }

    #[test]
    fn test_parse_preview_response_reports_unreadable_preview_objects() {
        match parse_preview_response(br#"{"title":5}"#) {
            Err(ScriptFilterPreviewError::Invalid(message)) => {
                assert!(message.starts_with("Invalid preview: "), "{message}");
            }
            other => panic!("unexpected result: {other:?}"),
        }
    }

    #[test]
    fn test_preview_request_runs_script_with_protocol_env() {
        let config = crate::ScriptFilterConfig {
            name: "Echo".into(),
            command: "sh".into(),
            keyword: "e".into(),
            icon: None,
            args: vec![
                "-c".into(),
                r#"[ "$RAFFI_PREVIEW" = 1 ] && printf '{"text":"%s|%s|%s|%s|%s"}' "$RAFFI_VALUE" "$RAFFI_TITLE" "$1" "$PANE" "$RAFFI_PREVIEW_WIDTH""#.into(),
                "sh".into(),
            ],
            action: None,
            secondary_action: None,
            env: std::collections::HashMap::new(),
            min_query_length: None,
            dynamic_preview: None,
            close_on_action: None,
            cache_seconds: None,
            cache_max_seconds: None,
            warm_queries: Vec::new(),
            warm_interval_seconds: None,
            sort_by_usage: None,
            actions: Vec::new(),
        };
        let item = super::ScriptFilterItem {
            actions: Vec::new(),
            time: None,
            children: None,
            children_lazy: false,
            title: "\u{1b}[32mAgent\u{1b}[0m".into(),
            valid: true,
            subtitle: None,
            arg: Some("w1:p1".into()),
            indent: 0,
            icon: None,
            preview: None,
            variables: [("PANE".to_string(), "p1".to_string())].into(),
            quicklookurl: None,
            text: None,
        };
        let preview = super::PreviewRequest::new(&config, "my query", &item, (400, 600))
            .run()
            .expect("preview run")
            .expect("preview object");
        assert!(matches!(
            preview.bodies.as_slice(),
            [PreparedPreviewBody::Text { content, .. }] if content == "w1:p1|Agent|my query|p1|400"
        ));
    }

    #[test]
    fn test_parse_script_filter_output_invalid_json() {
        let result = parse_script_filter_output("{not json}", None);
        assert!(result.is_err());
    }

    fn times(stdout: &str) -> Vec<Option<(i64, Option<String>)>> {
        parse_script_filter_output(stdout, None)
            .expect("the list still parses")
            .items
            .into_iter()
            .map(|item| item.time.map(|time| (time.at, time.label)))
            .collect()
    }

    #[test]
    fn test_item_time_accepts_rfc3339_epoch_and_object() {
        let now = super::super::item_time::now_unix();
        let rfc = jiff::Timestamp::from_second(now - 3600).unwrap();
        let stdout = format!(
            r#"{{"items":[
                {{"title":"a","time":"{rfc}"}},
                {{"title":"b","time":{epoch}}},
                {{"title":"c","time":{{"at":"{rfc}","label":"pushed"}}}},
                {{"title":"d"}}
            ]}}"#,
            epoch = now - 60
        );
        assert_eq!(
            times(&stdout),
            [
                Some((now - 3600, None)),
                Some((now - 60, None)),
                Some((now - 3600, Some("pushed".to_string()))),
                None,
            ]
        );
    }

    #[test]
    fn test_invalid_item_time_is_ignored_not_fatal() {
        for bad in [
            r#""yesterday""#,
            "1e400",
            "-1e400",
            "1.5",
            "[1]",
            "{}",
            r#"{"at":"x"}"#,
            "null",
            "true",
            "99999999999999999999999",
        ] {
            let stdout = format!(
                r#"{{"items":[{{"title":"kept","time":{bad},"children":[{{"title":"child","time":{bad}}}]}}]}}"#
            );
            let result = parse_script_filter_output(&stdout, None)
                .unwrap_or_else(|error| panic!("{bad} must not fail the list: {error}"));
            assert_eq!(result.items.len(), 1, "{bad}");
            assert!(result.items[0].time.is_none(), "{bad}");
            assert_eq!(result.items[0].children.as_ref().map(Vec::len), Some(1));
        }
    }

    fn hint(stdout: &str) -> Option<u64> {
        parse_script_filter_output(stdout, None)
            .expect("the list still parses")
            .cache_hint
    }

    #[test]
    fn test_cache_hint_is_read_leniently() {
        assert_eq!(
            hint(r#"{"cache":{"seconds":86400},"items":[]}"#),
            Some(86400)
        );
        assert_eq!(hint(r#"{"cache":90,"items":[]}"#), Some(90));
        assert_eq!(hint(r#"{"cache":{"seconds":0},"items":[]}"#), Some(0));
        assert_eq!(
            hint(r#"{"cache":{"seconds":60,"loosereload":true},"items":[]}"#),
            Some(60)
        );
        for bad in [
            r#""soon""#,
            "-1",
            "1.5",
            "60.0",
            "1e400",
            "{}",
            "null",
            "[60]",
            r#"{"seconds":"60"}"#,
            r#"{"seconds":-5}"#,
            "true",
        ] {
            let stdout = format!(r#"{{"cache":{bad},"items":[{{"title":"kept"}}]}}"#);
            let result = parse_script_filter_output(&stdout, None)
                .unwrap_or_else(|error| panic!("{bad} must not fail the list: {error}"));
            assert_eq!(result.cache_hint, None, "{bad}");
            assert!(!result.not_stored, "{bad}");
            assert_eq!(result.items.len(), 1, "{bad}");
        }
        assert_eq!(hint(r#"{"items":[]}"#), None);
    }

    fn warm(stdout: &str) -> Vec<String> {
        parse_script_filter_output(stdout, None)
            .expect("the list still parses")
            .cache_warm
    }

    #[test]
    fn test_warm_queries_are_read_leniently() {
        assert_eq!(
            warm(r#"{"cache":{"seconds":60,"warm":["","review"]},"items":[]}"#),
            ["", "review"]
        );
        assert_eq!(
            warm(r#"{"cache":{"warm":["a",1,null,["b"],"c"]},"items":[]}"#),
            ["a", "c"],
            "only the strings count"
        );
        for bad in [
            r#""review""#,
            "5",
            "{}",
            "null",
            r#"{"warm":"review"}"#,
            r#"{"warm":7}"#,
        ] {
            let stdout = format!(r#"{{"cache":{bad},"items":[{{"title":"kept"}}]}}"#);
            assert!(warm(&stdout).is_empty(), "{bad}");
        }
        assert!(warm(r#"{"items":[]}"#).is_empty());
        // A warm list does not disturb the seconds.
        assert_eq!(
            hint(r#"{"cache":{"seconds":90,"warm":["x"]},"items":[]}"#),
            Some(90)
        );
    }

    #[test]
    fn test_zero_cache_hint_marks_the_result_not_stored() {
        let result =
            parse_script_filter_output(r#"{"cache":{"seconds":0},"items":[]}"#, None).unwrap();
        assert!(result.not_stored);
        let result =
            parse_script_filter_output(r#"{"cache":{"seconds":5},"items":[]}"#, None).unwrap();
        assert!(!result.not_stored);
    }

    #[test]
    fn test_parse_script_filter_output_with_a_usage_sort_hint() {
        let asked =
            parse_script_filter_output(r#"{"sort":"usage","items":[]}"#, None).expect("valid json");
        assert!(asked.sort_by_usage);

        for stdout in [
            r#"{"items":[]}"#,
            r#"{"sort":"alpha","items":[]}"#,
            r#"{"sort":true,"items":[]}"#,
            r#"{"sort":null,"items":[]}"#,
        ] {
            let result = parse_script_filter_output(stdout, None).expect("valid json");
            assert!(!result.sort_by_usage, "{stdout} keeps the script order");
        }

        // A hint serde cannot hold as a value is ignored, not the whole list.
        let odd = parse_script_filter_output(r#"{"sort":1e400,"items":[{"title":"kept"}]}"#, None)
            .expect("an odd sort hint keeps the list");
        assert!(!odd.sort_by_usage);
        assert_eq!(odd.items.len(), 1);
        assert_eq!(odd.items[0].title, "kept");
    }

    #[test]
    fn test_snapshot_keeps_the_usage_sort_hint() {
        let path = temp_snapshot_path("sort");
        run_printing(
            &path,
            r#"{"sort":"usage","cache":{"seconds":300},"items":[{"title":"One"}]}"#,
        );
        let snapshot = super::cache::read_snapshot_path(&path).unwrap();
        let result = super::cache::parse_list(&snapshot, None).expect("cached result parses");
        assert!(
            result.sort_by_usage,
            "a cached payload is re-parsed, hint included"
        );
    }

    fn temp_snapshot_path(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "raffi-script-hint-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("list.json")
    }

    fn run_printing(path: &std::path::Path, output: &str) -> super::ScriptFilterResult {
        let target = super::SnapshotTarget {
            path: path.to_path_buf(),
            ttl: std::time::Duration::from_secs(300),
            keep: 10,
            publication: None,
        };
        super::run_script_filter(
            "sh",
            &["-c".into(), format!("printf '%s' '{output}'")],
            &Default::default(),
            "",
            None,
            Some(target),
            None,
        )
        .unwrap()
    }

    #[test]
    fn test_snapshot_keeps_the_hint_as_the_script_gave_it() {
        let path = temp_snapshot_path("kept");
        run_printing(&path, r#"{"cache":{"seconds":86400},"items":[]}"#);
        let snapshot = super::cache::read_snapshot_path(&path).unwrap();
        assert_eq!(snapshot.hint_secs, Some(86400));
        assert_eq!(snapshot.ttl_secs, 300);
    }

    #[test]
    fn test_zero_cache_hint_keeps_the_previous_snapshot() {
        let path = temp_snapshot_path("zero");
        run_printing(&path, r#"{"items":[{"title":"good"}]}"#);
        let before = std::fs::read(&path).unwrap();
        let result = run_printing(
            &path,
            r#"{"cache":{"seconds":0},"items":[{"title":"GitHub error"}]}"#,
        );
        assert!(result.not_stored);
        assert_eq!(result.items[0].title, "GitHub error");
        let snapshot = super::cache::read_snapshot_path(&path).unwrap();
        assert!(snapshot.payload.contains("good"));
        assert!(!snapshot.payload.contains("GitHub error"));
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "not even its metadata"
        );
    }

    #[test]
    fn test_zero_cache_hint_writes_nothing_when_nothing_was_cached() {
        let path = temp_snapshot_path("zero-cold");
        run_printing(&path, r#"{"cache":{"seconds":0},"items":[]}"#);
        assert!(!path.exists());
    }

    #[test]
    fn test_invalid_output_does_not_overwrite_a_valid_snapshot() {
        let dir = std::env::temp_dir().join(format!(
            "raffi-script-cache-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("list.json");
        let target = || {
            Some(super::SnapshotTarget {
                path: path.clone(),
                ttl: std::time::Duration::from_secs(60),
                keep: 10,
                publication: None,
            })
        };

        let valid = super::run_script_filter(
            "sh",
            &["-c".into(), r#"printf '{"items":[{"title":"ok"}]}'"#.into()],
            &Default::default(),
            "",
            None,
            target(),
            None,
        )
        .unwrap();
        assert_eq!(valid.items[0].title, "ok");
        assert!(super::cache::read_snapshot_path(&path).is_some());

        let invalid = super::run_script_filter(
            "sh",
            &["-c".into(), "printf 'not json'".into()],
            &Default::default(),
            "",
            None,
            target(),
            None,
        );
        assert!(invalid.is_err());
        let snapshot = super::cache::read_snapshot_path(&path).unwrap();
        assert!(snapshot.payload.contains("\"title\":\"ok\""));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_parse_children_lazy_field() {
        let result = super::parse_script_filter_output(
            r#"{"items":[{"title":"Lazy","children_lazy":true},{"title":"Plain"}]}"#,
            None,
        )
        .unwrap();
        assert!(result.items[0].children_lazy);
        assert!(result.items[0].has_children());
        assert!(!result.items[1].children_lazy);
        assert!(!result.items[1].has_children());
    }

    #[test]
    fn test_children_request_env_and_run() {
        let mut config_env = std::collections::HashMap::new();
        config_env.insert("SHARED".into(), "from-config".into());
        config_env.insert("ONLY_CONFIG".into(), "yes".into());
        let config = crate::ScriptFilterConfig {
            name: "Repos".into(),
            keyword: "r".into(),
            command: "sh".into(),
            args: vec![
                "-c".into(),
                r#"[ "$RAFFI_CHILDREN" = 1 ] && printf '{"items":[{"title":"%s|%s|%s|%s|%s|%s"}],"cache":45}' "$RAFFI_VALUE" "$RAFFI_TITLE" "$RAFFI_QUERY" "$SHARED" "$ONLY_CONFIG" "$RAFFI_REFRESH""#.into(),
                "sh".into(),
            ],
            env: config_env,
            ..Default::default()
        };
        let mut variables = std::collections::HashMap::new();
        variables.insert("SHARED".into(), "from-item".into());
        let item = super::ScriptFilterItem {
            title: "\x1b[32mPull requests\x1b[0m".into(),
            time: None,
            actions: Vec::new(),
            children: None,
            children_lazy: true,
            valid: true,
            subtitle: None,
            arg: Some("owner/repo#pulls".into()),
            icon: None,
            indent: 0,
            preview: None,
            variables,
            quicklookurl: None,
            text: None,
        };

        let request = super::ChildrenRequest::new(&config, "raffi", &item, true);
        assert_eq!(
            request.env.get("RAFFI_CHILDREN").map(String::as_str),
            Some("1")
        );
        assert_eq!(
            request.env.get("RAFFI_REFRESH").map(String::as_str),
            Some("1")
        );

        let result = request.run().unwrap();
        assert_eq!(result.cache_hint, Some(45));
        assert_eq!(result.items.len(), 1);
        assert_eq!(
            result.items[0].title,
            "owner/repo#pulls|Pull requests|raffi|from-item|yes|1"
        );
    }
}
