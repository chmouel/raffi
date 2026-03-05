use std::process::{Command, Stdio};

use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;
use iced::Task;
use serde::Deserialize;

use super::state::Message;
use crate::TextSnippet;

#[derive(Debug, Deserialize)]
struct SnippetCommandItem {
    title: String,
    arg: Option<String>,
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
            let output = Command::new(&command)
                .args(&args)
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .output();

            match output {
                Ok(output) if output.status.success() => {
                    let stdout = String::from_utf8_lossy(&output.stdout);
                    match serde_json::from_str::<SnippetCommandResponse>(&stdout) {
                        Ok(response) => Ok(response
                            .items
                            .into_iter()
                            .map(|item| TextSnippet {
                                name: item.title,
                                value: item.arg.unwrap_or_default(),
                            })
                            .collect()),
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

pub(super) fn filter_snippets(snippets: &[TextSnippet], query: &str) -> Vec<usize> {
    if query.is_empty() {
        return (0..snippets.len()).collect();
    }

    let matcher = SkimMatcherV2::default();
    let mut scored: Vec<(usize, i64)> = snippets
        .iter()
        .enumerate()
        .filter_map(|(index, snippet)| {
            matcher
                .fuzzy_match(&snippet.name, query)
                .map(|score| (index, score))
        })
        .collect();
    scored.sort_by(|a, b| b.1.cmp(&a.1));
    scored.into_iter().map(|(index, _)| index).collect()
}

#[cfg(test)]
mod tests {
    use super::filter_snippets;
    use crate::TextSnippet;

    #[test]
    fn test_filter_snippets_empty_query_returns_all() {
        let snippets = vec![
            TextSnippet {
                name: "Alpha".to_string(),
                value: "a".to_string(),
            },
            TextSnippet {
                name: "Beta".to_string(),
                value: "b".to_string(),
            },
            TextSnippet {
                name: "Gamma".to_string(),
                value: "c".to_string(),
            },
        ];

        let result = filter_snippets(&snippets, "");
        assert_eq!(result, vec![0, 1, 2]);
    }

    #[test]
    fn test_filter_snippets_partial_query() {
        let snippets = vec![
            TextSnippet {
                name: "Personal Email".to_string(),
                value: "user@example.com".to_string(),
            },
            TextSnippet {
                name: "Work Email".to_string(),
                value: "user@company.com".to_string(),
            },
            TextSnippet {
                name: "Phone Number".to_string(),
                value: "+1234567890".to_string(),
            },
        ];

        let result = filter_snippets(&snippets, "work");
        assert!(!result.is_empty());
        assert!(result.contains(&1));
    }

    #[test]
    fn test_filter_snippets_no_match() {
        let snippets = vec![
            TextSnippet {
                name: "Alpha".to_string(),
                value: "a".to_string(),
            },
            TextSnippet {
                name: "Beta".to_string(),
                value: "b".to_string(),
            },
        ];

        let result = filter_snippets(&snippets, "zzzzz");
        assert!(result.is_empty());
    }
}
