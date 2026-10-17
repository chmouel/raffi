//! File browser previews produced by the user's `preview_command`.
//!
//! The command gets the selected path and prints ANSI text or a Script Filter
//! preview object. The built-in preview stays on screen until it answers.

use std::collections::HashMap;
use std::process::Command;
use std::time::{Duration, SystemTime};

use iced::Task;

use super::state::LauncherApp;
use super::state::Message;
use super::types::PreparedPreview;

/// Longest a preview command may run before it is killed.
const PREVIEW_TIMEOUT: Duration = Duration::from_secs(3);
/// Largest output accepted, enough for a base64 image.
const MAX_PREVIEW_OUTPUT: usize = 8 * 1024 * 1024;
/// Text output beyond these limits is cut, to keep the pane responsive.
const MAX_TEXT_LINES: usize = 1000;
const MAX_TEXT_BYTES: usize = 256 * 1024;
/// Wait after a selection change before running the command, so scrolling
/// through a directory does not start one process per row.
const PREVIEW_DEBOUNCE: Duration = Duration::from_millis(80);
/// Previews kept for the session.
pub(super) const PREVIEW_CACHE_SIZE: usize = 64;
/// Monospace cell size relative to the font size, to tell the command how
/// many columns and lines fit in the pane.
const CELL_WIDTH_RATIO: f32 = 0.6;
const CELL_HEIGHT_RATIO: f32 = 1.3;
/// Horizontal padding of the text block the output is drawn in.
const TEXT_BLOCK_PADDING: f32 = 24.0;

/// Fields of a Script Filter preview object. A JSON object using only these
/// keys is a preview; any other JSON (such as a `.json` file printed as is)
/// is shown as text.
const PREVIEW_OBJECT_KEYS: &[&str] = &[
    "title", "subtitle", "badge", "icon", "text", "image", "sections", "metadata", "refresh",
    "dynamic",
];

/// A path as it was when previewed, so edits get a fresh preview.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FilePreviewKey {
    pub path: String,
    modified: Option<SystemTime>,
    len: u64,
}

impl FilePreviewKey {
    pub fn of(path: &str) -> Self {
        let meta = std::fs::metadata(path).ok();
        Self {
            path: path.to_string(),
            modified: meta.as_ref().and_then(|meta| meta.modified().ok()),
            len: meta.map_or(0, |meta| meta.len()),
        }
    }
}

/// What the preview command printed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FilePreviewOutput {
    /// ANSI text, already cut to the display limits.
    Text(String),
    Rich(Box<PreparedPreview>),
}

/// Result of one run: `Ok(None)` when the command has no preview for the
/// path (no output or a non-zero exit) and the built-in one is kept.
pub(crate) type FilePreviewResult = Result<Option<FilePreviewOutput>, String>;

/// Read the command output: a preview object, ANSI text, or nothing.
pub(super) fn parse_output(success: bool, stdout: &[u8]) -> FilePreviewResult {
    if !success {
        return Ok(None);
    }
    let text = String::from_utf8_lossy(stdout);
    if text.trim().is_empty() {
        return Ok(None);
    }
    if let Ok(serde_json::Value::Object(object)) = serde_json::from_str(&text) {
        if !object.is_empty()
            && object
                .keys()
                .all(|key| PREVIEW_OBJECT_KEYS.contains(&key.as_str()))
        {
            let spec: super::types::RawPreviewSpec =
                serde_json::from_value(serde_json::Value::Object(object))
                    .map_err(|error| format!("Invalid preview: {error}"))?;
            return Ok(Some(FilePreviewOutput::Rich(Box::new(spec.into()))));
        }
    }
    Ok(Some(FilePreviewOutput::Text(cap_text(&text))))
}

/// Cut `text` to the display limits, noting what was left out.
fn cap_text(text: &str) -> String {
    let text = text.trim_end_matches(['\n', '\r']);
    let mut end = text.len();
    let mut cut = false;
    if let Some((index, _)) = text.match_indices('\n').nth(MAX_TEXT_LINES - 1) {
        end = index;
        cut = true;
    }
    if end > MAX_TEXT_BYTES {
        end = MAX_TEXT_BYTES;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        cut = true;
    }
    if !cut {
        return text.to_string();
    }
    // Reset styles first: the cut may fall inside a coloured run.
    format!("{}\x1b[0m\n\x1b[2m… output truncated\x1b[0m", &text[..end])
}

/// Everything needed to run the preview command once.
#[derive(Debug, Clone)]
pub(super) struct FilePreviewRequest {
    command: String,
    env: HashMap<String, String>,
}

impl FilePreviewRequest {
    /// `pane` is the preview pane size in pixels and `font_size` the size of
    /// the monospace text the output is drawn with.
    pub fn new(command: &str, path: &str, pane: (f32, f32), font_size: f32) -> Self {
        // One spare column: glyph advances vary between fonts, and a line
        // that overflows by a hair would wrap a second time in the pane.
        let columns = (pane.0 - TEXT_BLOCK_PADDING) / (font_size * CELL_WIDTH_RATIO) - 1.0;
        let lines = pane.1 / (font_size * CELL_HEIGHT_RATIO);
        let env = HashMap::from([
            ("RAFFI_PREVIEW".into(), "1".into()),
            ("RAFFI_VALUE".into(), path.to_string()),
            (
                "RAFFI_PREVIEW_WIDTH".into(),
                (pane.0.round() as u32).to_string(),
            ),
            (
                "RAFFI_PREVIEW_HEIGHT".into(),
                (pane.1.round() as u32).to_string(),
            ),
            (
                "RAFFI_PREVIEW_COLUMNS".into(),
                (columns.floor().max(20.0) as u32).to_string(),
            ),
            (
                "RAFFI_PREVIEW_LINES".into(),
                (lines.floor().max(5.0) as u32).to_string(),
            ),
        ]);
        Self {
            command: command.to_string(),
            env,
        }
    }

    /// The path is passed through the environment, never pasted into the
    /// shell line, so any file name is safe.
    fn shell_line(&self) -> String {
        format!("{} \"$RAFFI_VALUE\"", self.command)
    }

    pub fn run(self) -> FilePreviewResult {
        crate::debug_log!(
            "file_preview: command={:?} path={:?}",
            self.command,
            self.env.get("RAFFI_VALUE")
        );
        let mut command = Command::new("sh");
        command.arg("-c").arg(self.shell_line()).envs(&self.env);
        let (status, output) =
            super::actions::run_bounded_status(command, PREVIEW_TIMEOUT, MAX_PREVIEW_OUTPUT)?;
        let result = parse_output(status.success(), &output);
        if !status.success() {
            crate::debug_log!("file_preview: exited with {status}, using built-in preview");
        }
        result
    }
}

impl LauncherApp {
    /// Path of the selected file browser entry when the preview command
    /// should describe it.
    fn file_preview_target(&self) -> Option<String> {
        if !self.view.show_preview || !self.file_browser.active {
            return None;
        }
        self.addons.file_browser.preview_command.as_ref()?;
        self.selected_file_to_inspect().map(|(path, _)| path)
    }

    /// Follow the selection: when it moved to another entry, schedule a run
    /// unless a current preview of it is cached. Called after every update.
    pub(super) fn ensure_file_browser_preview(&mut self) -> Task<Message> {
        let target = self.file_preview_target();
        let state = &mut self.file_browser;
        if target == state.preview_target {
            return Task::none();
        }
        state.preview_target = target.clone();
        state.preview_due = false;
        let Some(path) = target else {
            return Task::none();
        };
        let key = FilePreviewKey::of(&path);
        let fresh = state
            .script_previews
            .iter()
            .any(|(cached, result)| *cached == key && result.is_ok());
        if fresh {
            return Task::none();
        }
        state
            .script_previews
            .retain(|(cached, _)| cached.path != path);
        Task::perform(
            async move {
                let (sender, receiver) = iced::futures::channel::oneshot::channel();
                std::thread::spawn(move || {
                    std::thread::sleep(PREVIEW_DEBOUNCE);
                    let _ = sender.send(());
                });
                let _ = receiver.await;
                path
            },
            Message::FileBrowserPreviewDue,
        )
    }

    pub(super) fn handle_file_browser_preview_due(&mut self, path: String) -> Task<Message> {
        if self.file_browser.preview_target.as_deref() != Some(path.as_str()) {
            return Task::none();
        }
        self.file_browser.preview_due = true;
        self.start_file_browser_preview()
    }

    /// Run the command for the selected entry once its debounce is over and
    /// no other run is out.
    fn start_file_browser_preview(&mut self) -> Task<Message> {
        let state = &self.file_browser;
        if !state.preview_due || state.preview_inflight {
            return Task::none();
        }
        let (Some(path), Some(command)) = (
            state.preview_target.clone(),
            self.addons.file_browser.preview_command.clone(),
        ) else {
            return Task::none();
        };
        if state.script_preview(&path).is_some() {
            return Task::none();
        }
        let pane = (
            self.preview_viewport_width(),
            self.preview_viewport_height(),
        );
        let mut request = FilePreviewRequest::new(
            &crate::expand_config_value(&command),
            &path,
            pane,
            self.view.preview_style.text,
        );
        request.env.extend(self.view.theme.command_env());
        let key = FilePreviewKey::of(&path);
        self.file_browser.preview_inflight = true;
        Task::perform(
            async move {
                let (sender, receiver) = iced::futures::channel::oneshot::channel();
                std::thread::spawn(move || {
                    let _ = sender.send(request.run());
                });
                let result = receiver
                    .await
                    .unwrap_or_else(|_| Err("preview worker stopped".into()));
                (key, result)
            },
            |(key, result)| Message::FileBrowserPreview(key, result),
        )
    }

    pub(super) fn handle_file_browser_preview(
        &mut self,
        key: FilePreviewKey,
        result: FilePreviewResult,
    ) -> Task<Message> {
        let state = &mut self.file_browser;
        state.preview_inflight = false;
        if let Err(error) = &result {
            crate::debug_log!("file_preview: {error}");
        }
        state
            .script_previews
            .retain(|(cached, _)| cached.path != key.path);
        if state.script_previews.len() >= PREVIEW_CACHE_SIZE {
            state.script_previews.drain(..1);
        }
        state.script_previews.push((key, result));
        // The selection may have moved on while this one ran.
        self.start_file_browser_preview()
    }
}

#[cfg(test)]
mod tests {
    use super::super::types::PreparedPreviewBody;
    use super::{parse_output, FilePreviewOutput, FilePreviewRequest, MAX_TEXT_LINES};

    #[test]
    fn test_parse_output_reads_text_preview_objects_and_fallbacks() {
        assert_eq!(parse_output(true, b"  \n"), Ok(None));
        assert_eq!(parse_output(false, b"No preview"), Ok(None));
        assert_eq!(
            parse_output(true, b"\x1b[31mred\x1b[0m\n\n"),
            Ok(Some(FilePreviewOutput::Text("\x1b[31mred\x1b[0m".into())))
        );

        let Ok(Some(FilePreviewOutput::Rich(preview))) = parse_output(
            true,
            br#"{"badge":"PDF","image":"/tmp/p.jpg","metadata":[["Pages","3"]]}"#,
        ) else {
            panic!("expected a preview object");
        };
        assert_eq!(preview.badge.as_deref(), Some("PDF"));
        assert_eq!(
            preview.bodies,
            vec![PreparedPreviewBody::Image("/tmp/p.jpg".into())]
        );
        assert_eq!(preview.metadata, vec![("Pages".into(), "3".into())]);

        assert!(parse_output(true, br#"{"metadata": "oops"}"#).is_err());
    }

    #[test]
    fn test_plain_json_files_are_shown_as_text() {
        let package = br#"{"name": "demo", "title": "x"}"#;
        assert_eq!(
            parse_output(true, package),
            Ok(Some(FilePreviewOutput::Text(
                String::from_utf8_lossy(package).into()
            )))
        );
        assert!(matches!(
            parse_output(true, b"{}"),
            Ok(Some(FilePreviewOutput::Text(_)))
        ));
    }

    #[test]
    fn test_long_text_is_truncated_with_a_note() {
        let long = "line\n".repeat(MAX_TEXT_LINES + 50);
        let Ok(Some(FilePreviewOutput::Text(text))) = parse_output(true, long.as_bytes()) else {
            panic!("expected text");
        };
        assert_eq!(text.matches("line").count(), MAX_TEXT_LINES);
        assert!(text.ends_with("… output truncated\x1b[0m"));
    }

    #[test]
    fn test_hostile_file_names_reach_the_command_verbatim() {
        let dir = std::env::temp_dir().join(format!("raffi-preview-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("$(touch pwned) `id` \"q\" 'a b'.txt");
        std::fs::write(&path, "").unwrap();
        let path = path.to_string_lossy().to_string();

        let request = FilePreviewRequest::new("printf '%s'", &path, (700.0, 800.0), 16.0);
        assert_eq!(request.run(), Ok(Some(FilePreviewOutput::Text(path))));
        assert!(!dir.join("pwned").exists());

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_request_reports_the_pane_in_cells() {
        let request =
            FilePreviewRequest::new("env", "/tmp/x", (24.0 + 9.6 * 70.0, 20.8 * 40.0), 16.0);
        assert_eq!(request.env["RAFFI_PREVIEW_COLUMNS"], "69");
        assert_eq!(request.env["RAFFI_PREVIEW_LINES"], "40");
        assert_eq!(request.env["RAFFI_PREVIEW"], "1");
    }
}
