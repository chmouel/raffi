use std::borrow::Cow;
use std::collections::HashMap;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use iced::Task;
use serde::Deserialize;

use super::cache::{self, SnapshotTarget};
use super::state::{Message, ScriptFilterResult};
use super::types::{
    PreparedPreview, RawPreviewSpec, ScriptFilterItem, ScriptFilterPreviewError, ScriptFilterView,
};

/// Longest a `RAFFI_PREVIEW=1` run may take before it is killed.
const PREVIEW_TIMEOUT: Duration = Duration::from_secs(3);
/// Largest preview output accepted, enough for a base64 screenshot.
const MAX_PREVIEW_OUTPUT: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Deserialize)]
struct ScriptFilterResponse {
    items: Vec<ScriptFilterItem>,
    #[serde(default)]
    views: Vec<ScriptFilterView>,
    #[serde(default)]
    view: Option<String>,
}

pub(super) fn parse_script_filter_output(
    stdout: &str,
    default_icon: Option<String>,
) -> Result<ScriptFilterResult, String> {
    serde_json::from_str::<ScriptFilterResponse>(stdout)
        .map(|response| ScriptFilterResult {
            items: response.items,
            default_icon,
            views: response.views,
            view: response.view,
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
        async move { run_script_filter(&command, &args, &env, &query, default_icon, cache) },
        move |result| Message::ScriptFilterResult(generation, result),
    )
}

/// Run a script filter for `query` and parse its items. Blocking.
pub(super) fn run_script_filter(
    command: &str,
    args: &[String],
    env: &HashMap<String, String>,
    query: &str,
    default_icon: Option<String>,
    cache: Option<SnapshotTarget>,
) -> Result<ScriptFilterResult, String> {
    crate::debug_log!("script_filter: executing command={command:?} args={args:?} query={query:?}");
    let output = Command::new(command)
        .args(args)
        .envs(env)
        .arg(query)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();

    match output {
        Ok(output) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            match parse_script_filter_output(&stdout, default_icon) {
                Ok(result) => {
                    if let Some(target) = cache {
                        if let Err(error) = cache::write_snapshot_path(
                            &target.path,
                            &stdout,
                            target.ttl,
                            target.keep,
                        ) {
                            crate::debug_log!("script_filter: cannot write cache: {error}");
                        }
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
        Ok(output) => {
            eprintln!(
                "Script filter: {} exited with status {}",
                command, output.status
            );
            crate::debug_log!(
                "script_filter: {command:?} exited with status {}",
                output.status
            );
            Err(format!("Script exited with status {}", output.status))
        }
        Err(error) => {
            eprintln!("Script filter: failed to execute {}: {}", command, error);
            crate::debug_log!("script_filter: failed to execute {command:?}: {error}");
            Err(format!("Failed to execute: {}", error))
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
        let mut command = Command::new(&self.command);
        command.args(&self.args).arg(&self.query).envs(&self.env);
        let output = super::actions::run_bounded(command, PREVIEW_TIMEOUT, MAX_PREVIEW_OUTPUT)
            .map_err(ScriptFilterPreviewError::Run)?;
        let preview = parse_preview_response(&output)?;
        if preview.is_some() {
            if let Some(target) = self.cache {
                let payload = String::from_utf8_lossy(&output);
                if let Err(error) =
                    cache::write_snapshot_path(&target.path, &payload, target.ttl, target.keep)
                {
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
        let run =
            super::run_script_filter(&config.command, &config.args, &config.env, "", None, None)
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
            warm_queries: Vec::new(),
            warm_interval_seconds: None,
        };
        let item = super::ScriptFilterItem {
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
            })
        };

        let valid = super::run_script_filter(
            "sh",
            &["-c".into(), r#"printf '{"items":[{"title":"ok"}]}'"#.into()],
            &Default::default(),
            "",
            None,
            target(),
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
        );
        assert!(invalid.is_err());
        let snapshot = super::cache::read_snapshot_path(&path).unwrap();
        assert!(snapshot.payload.contains("\"title\":\"ok\""));

        let _ = std::fs::remove_dir_all(dir);
    }
}
