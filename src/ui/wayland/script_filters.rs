use std::process::{Command, Stdio};

use iced::Task;
use serde::Deserialize;

use super::state::{Message, ScriptFilterResult};
use super::types::ScriptFilterItem;

#[derive(Debug, Clone, Deserialize)]
struct ScriptFilterResponse {
    items: Vec<ScriptFilterItem>,
}

pub(super) fn parse_script_filter_output(
    stdout: &str,
    default_icon: Option<String>,
) -> Result<ScriptFilterResult, String> {
    serde_json::from_str::<ScriptFilterResponse>(stdout)
        .map(|response| ScriptFilterResult {
            items: response.items,
            default_icon,
        })
        .map_err(|error| format!("Invalid JSON: {}", error))
}

pub(super) fn execute_script_filter(
    command: String,
    args: Vec<String>,
    env: std::collections::HashMap<String, String>,
    query: String,
    generation: u64,
    default_icon: Option<String>,
) -> Task<Message> {
    Task::perform(
        async move {
            crate::debug_log!(
                "script_filter: executing command={command:?} args={args:?} query={query:?}"
            );
            let output = Command::new(&command)
                .args(&args)
                .envs(&env)
                .arg(&query)
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .output();

            match output {
                Ok(output) if output.status.success() => {
                    let stdout = String::from_utf8_lossy(&output.stdout);
                    match parse_script_filter_output(&stdout, default_icon) {
                        Ok(result) => {
                            crate::debug_log!(
                                "script_filter: got {} items from {command:?}",
                                result.items.len()
                            );
                            Ok(result)
                        }
                        Err(error) => {
                            eprintln!("Script filter: invalid JSON from {}: {}", command, error);
                            crate::debug_log!(
                                "script_filter: invalid JSON from {command:?}: {error}"
                            );
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
        },
        move |result| Message::ScriptFilterResult(generation, result),
    )
}

#[cfg(test)]
mod tests {
    use super::parse_script_filter_output;

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
        assert_eq!(item.preview.as_deref(), Some("Detailed PR body\nLine 2"));
        assert_eq!(
            item.quicklookurl.as_deref(),
            Some("https://github.com/chmouel/raffi/pull/42")
        );
        let text = item.text.as_ref().expect("expected text object");
        assert_eq!(text.copy.as_deref(), Some("#42"));
        assert_eq!(text.largetype.as_deref(), Some("Large type text"));
    }

    #[test]
    fn test_parse_script_filter_output_invalid_json() {
        let result = parse_script_filter_output("{not json}", None);
        assert!(result.is_err());
    }
}
