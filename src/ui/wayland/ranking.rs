use std::path::Path;

use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;
use unicode_normalization::char::is_combining_mark;
use unicode_normalization::UnicodeNormalization;

use crate::RaffiConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SearchFieldKind {
    Title,
    Executable,
    Keyword,
    Subtitle,
    Metadata,
}

impl SearchFieldKind {
    pub const fn weight_percent(self) -> u32 {
        match self {
            Self::Title | Self::Executable => 100,
            Self::Keyword => 70,
            Self::Subtitle => 50,
            Self::Metadata => 40,
        }
    }

    pub const fn is_primary(self) -> bool {
        matches!(self, Self::Title | Self::Executable)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SearchField {
    pub kind: SearchFieldKind,
    pub normalized: String,
    word_ranges: Vec<(usize, usize)>,
    word_initials: String,
}

impl SearchField {
    pub fn new(kind: SearchFieldKind, raw: &str) -> Option<Self> {
        let normalized = normalize_search_text(raw);
        if normalized.is_empty() {
            return None;
        }
        let (word_ranges, word_initials) = extract_word_ranges(&normalized);
        Some(Self {
            kind,
            normalized,
            word_ranges,
            word_initials,
        })
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct SearchDocument {
    fields: Vec<SearchField>,
}

impl SearchDocument {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, kind: SearchFieldKind, raw: &str) {
        let Some(field) = SearchField::new(kind, raw) else {
            return;
        };
        if self
            .fields
            .iter()
            .any(|existing| existing.kind == field.kind && existing.normalized == field.normalized)
        {
            return;
        }
        self.fields.push(field);
    }

    #[cfg(test)]
    pub fn with_field(mut self, kind: SearchFieldKind, raw: &str) -> Self {
        self.push(kind, raw);
        self
    }

    pub fn from_launcher(config: &RaffiConfig) -> Self {
        let mut doc = Self::new();
        let title = config
            .description
            .as_deref()
            .or(config.binary.as_deref())
            .unwrap_or_default();
        doc.push(SearchFieldKind::Title, title);
        if let Some(binary) = config.binary.as_deref() {
            let basename = executable_basename(binary);
            doc.push(SearchFieldKind::Executable, basename);
        }
        doc
    }

    pub fn from_command_item(item: &super::command_list::CommandItem) -> Self {
        if item.match_text.is_empty() {
            return Self::new();
        }
        let mut doc = Self::new();
        if item
            .match_text
            .contains(super::command_list::MATCH_FIELD_SEPARATOR)
        {
            let mut fields = item
                .match_text
                .split(super::command_list::MATCH_FIELD_SEPARATOR);
            let url = fields.next().unwrap_or_default();
            let parsed_title = fields.next().unwrap_or(&item.title);
            doc.push(
                SearchFieldKind::Title,
                if parsed_title.is_empty() {
                    &item.title
                } else {
                    parsed_title
                },
            );
            doc.push(SearchFieldKind::Metadata, url);
            for rest in fields {
                doc.push(SearchFieldKind::Metadata, rest);
            }
            return doc;
        }
        let clean_title = item.title.trim_start_matches(['●', '#', '@']).trim_start();
        doc.push(SearchFieldKind::Title, clean_title);
        if clean_title != item.title {
            doc.push(SearchFieldKind::Title, &item.title);
        }
        doc.push(SearchFieldKind::Subtitle, &item.subtitle);
        doc.push(SearchFieldKind::Metadata, &item.match_text);
        let audio_role = if item.identity.starts_with("sink/") {
            Some(
                if item
                    .icon_names
                    .iter()
                    .any(|icon| icon.contains("headphones"))
                {
                    "outputs headphones"
                } else {
                    "outputs speakers"
                },
            )
        } else if item.identity.starts_with("source/") {
            Some("inputs microphones")
        } else {
            None
        };
        if let Some(role) = audio_role {
            doc.push(SearchFieldKind::Keyword, role);
        }
        for (label, value) in &item.details {
            match label.as_str() {
                "Keyword" => doc.push(SearchFieldKind::Keyword, value),
                "App ID" | "Class" | "Player" | "Username" | "Handle" | "Name" | "Topic"
                | "Purpose" | "Artist" | "Browser" => doc.push(SearchFieldKind::Subtitle, value),
                "URL" | "Folder" | "Pid" | "Command" | "Port" | "Address" | "Type"
                | "Conversation" | "Path" | "Workspace" => {
                    doc.push(SearchFieldKind::Metadata, value)
                }
                _ => {}
            }
        }
        doc
    }

    pub fn from_snippet(snippet: &crate::TextSnippet) -> Self {
        let mut doc = Self::new();
        doc.push(SearchFieldKind::Title, &snippet.name);
        if let Some(keyword) = snippet.keyword.as_deref() {
            doc.push(SearchFieldKind::Keyword, keyword);
        }
        doc.push(SearchFieldKind::Subtitle, &snippet.value);
        doc
    }
}

pub(super) fn score_snippet_for_main_view(
    snippet: &crate::TextSnippet,
    document: &SearchDocument,
    query: &PreparedQuery,
    matcher: &SkimMatcherV2,
) -> Option<ScoredMatch> {
    if let Some(keyword) = snippet.keyword.as_deref().map(normalize_search_text) {
        if !keyword.is_empty() {
            if keyword == query.normalized {
                return Some(ScoredMatch {
                    quality: MatchQuality::ExactPrimary,
                    score: 10_000,
                });
            }
            if keyword.starts_with(&query.normalized) {
                return Some(ScoredMatch {
                    quality: MatchQuality::PrimaryWordPrefix,
                    score: 9_000,
                });
            }
        }
    }
    score_document(document, query, matcher)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum MatchQuality {
    DomainMatch,
    ExactPrimary,
    PrimaryWordPrefix,
    PrimaryMatch,
    MixedFields,
    SecondaryOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ScoredMatch {
    pub quality: MatchQuality,
    pub score: u32,
}

impl ScoredMatch {
    pub const fn domain() -> Self {
        Self {
            quality: MatchQuality::DomainMatch,
            score: 10_000,
        }
    }

    pub const fn sort_key(self) -> (MatchQuality, std::cmp::Reverse<u32>) {
        (self.quality, std::cmp::Reverse(self.score))
    }
}

#[derive(Debug, Clone)]
pub(super) struct PreparedQuery {
    pub normalized: String,
    pub terms: Vec<String>,
}

impl PreparedQuery {
    pub fn new(raw: &str) -> Option<Self> {
        let normalized = normalize_search_text(raw);
        if normalized.is_empty() {
            return None;
        }
        let mut terms = Vec::new();
        for term in normalized.split(' ').filter(|term| !term.is_empty()) {
            if !terms.iter().any(|existing| existing == term) {
                terms.push(term.to_string());
            }
        }
        if terms.is_empty() {
            return None;
        }
        Some(Self { normalized, terms })
    }
}

pub(super) fn executable_basename(binary: &str) -> &str {
    let trimmed = binary.trim();
    Path::new(trimmed)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or(trimmed)
}

pub(super) fn normalize_search_text(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut pending_space = false;
    if input.is_ascii() {
        for byte in input.bytes() {
            if byte.is_ascii_whitespace() {
                pending_space = !out.is_empty();
                continue;
            }
            if pending_space {
                out.push(' ');
                pending_space = false;
            }
            out.push(char::from(byte.to_ascii_lowercase()));
        }
        return out;
    }
    for ch in input.nfd().filter(|&ch| !is_combining_mark(ch)) {
        if ch.is_whitespace() {
            pending_space = !out.is_empty();
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        for lower in ch.to_lowercase() {
            out.push(lower);
        }
    }
    out
}

fn extract_word_ranges(normalized: &str) -> (Vec<(usize, usize)>, String) {
    let mut ranges = Vec::new();
    let mut initials = String::new();
    let mut word_start: Option<usize> = None;
    for (idx, ch) in normalized.char_indices() {
        if ch.is_alphanumeric() {
            if word_start.is_none() {
                word_start = Some(idx);
                initials.push(ch);
            }
        } else if let Some(start) = word_start.take() {
            ranges.push((start, idx));
        }
    }
    if let Some(start) = word_start {
        ranges.push((start, normalized.len()));
    }
    (ranges, initials)
}

pub(super) fn score_document(
    document: &SearchDocument,
    query: &PreparedQuery,
    matcher: &SkimMatcherV2,
) -> Option<ScoredMatch> {
    score_document_with_options(document, query, matcher, true)
}

pub(super) fn score_document_literal_only(
    document: &SearchDocument,
    query: &PreparedQuery,
    matcher: &SkimMatcherV2,
) -> Option<ScoredMatch> {
    score_document_with_options(document, query, matcher, false)
}

fn score_document_with_options(
    document: &SearchDocument,
    query: &PreparedQuery,
    matcher: &SkimMatcherV2,
    allow_non_literal_primary: bool,
) -> Option<ScoredMatch> {
    if document.fields.is_empty() {
        return None;
    }

    let exact_primary = document
        .fields
        .iter()
        .any(|field| field.kind.is_primary() && field.normalized == query.normalized);

    let mut all_primary_word_prefix = true;
    let mut all_primary_matched = true;
    let mut any_primary_matched = false;
    let mut weighted_sum = 0u32;

    for term in &query.terms {
        let mut best_weighted = 0u32;
        let mut term_has_primary = false;
        let mut term_has_primary_word_prefix = false;

        for field in &document.fields {
            let Some(term_match) =
                match_term_in_field(field, term, matcher, allow_non_literal_primary)
            else {
                continue;
            };
            if field.kind.is_primary() {
                term_has_primary = true;
                if term_match.is_literal_word_prefix {
                    term_has_primary_word_prefix = true;
                }
            }
            let weighted = term_match.base_score * field.kind.weight_percent();
            if weighted > best_weighted {
                best_weighted = weighted;
            }
        }

        if best_weighted == 0 {
            return None;
        }
        if !term_has_primary {
            all_primary_matched = false;
            all_primary_word_prefix = false;
        } else {
            any_primary_matched = true;
            if !term_has_primary_word_prefix {
                all_primary_word_prefix = false;
            }
        }
        weighted_sum += best_weighted;
    }

    let quality = if exact_primary {
        MatchQuality::ExactPrimary
    } else if all_primary_word_prefix {
        MatchQuality::PrimaryWordPrefix
    } else if all_primary_matched {
        MatchQuality::PrimaryMatch
    } else if any_primary_matched {
        MatchQuality::MixedFields
    } else {
        MatchQuality::SecondaryOnly
    };

    let score = if exact_primary {
        10_000
    } else {
        weighted_sum / query.terms.len() as u32
    };

    Some(ScoredMatch { quality, score })
}

#[derive(Debug, Clone, Copy)]
struct TermFieldMatch {
    base_score: u32,
    is_literal_word_prefix: bool,
}

fn match_term_in_field(
    field: &SearchField,
    term: &str,
    matcher: &SkimMatcherV2,
    allow_non_literal_primary: bool,
) -> Option<TermFieldMatch> {
    if field.normalized == term {
        return Some(TermFieldMatch {
            base_score: 100,
            is_literal_word_prefix: true,
        });
    }
    if field.normalized.starts_with(term) {
        return Some(TermFieldMatch {
            base_score: 90,
            is_literal_word_prefix: true,
        });
    }
    if has_word_prefix(&field.normalized, term) {
        return Some(TermFieldMatch {
            base_score: 85,
            is_literal_word_prefix: true,
        });
    }
    if allow_non_literal_primary
        && field.kind.is_primary()
        && has_abbreviation(&field.word_initials, term)
    {
        return Some(TermFieldMatch {
            base_score: 80,
            is_literal_word_prefix: false,
        });
    }
    if field.normalized.contains(term) {
        return Some(TermFieldMatch {
            base_score: 70,
            is_literal_word_prefix: false,
        });
    }
    if allow_non_literal_primary && field.kind.is_primary() {
        if let Some(skipped) =
            compact_subsequence_skips(&field.normalized, &field.word_ranges, term, matcher)
        {
            return Some(TermFieldMatch {
                base_score: 60u32.saturating_sub(skipped),
                is_literal_word_prefix: false,
            });
        }
    }
    None
}

pub(super) fn has_word_prefix(normalized_text: &str, normalized_term: &str) -> bool {
    if normalized_term.is_empty() {
        return false;
    }
    normalized_text
        .match_indices(normalized_term)
        .any(|(at, _)| {
            normalized_text[..at]
                .chars()
                .next_back()
                .is_none_or(|before| !before.is_alphanumeric())
        })
}

fn has_abbreviation(word_initials: &str, term: &str) -> bool {
    term.chars().count() >= 2 && word_initials.contains(term)
}

fn compact_subsequence_skips(
    normalized: &str,
    word_ranges: &[(usize, usize)],
    term: &str,
    matcher: &SkimMatcherV2,
) -> Option<u32> {
    let term_len = term.chars().count();
    if term_len < 3 || !term.chars().all(char::is_alphanumeric) {
        return None;
    }
    let max_span = 2 * term_len - 1;
    let mut best_skips: Option<u32> = None;

    for &(start, end) in word_ranges {
        let word = &normalized[start..end];
        if word.len() < term.len() || word.chars().count() < term_len {
            continue;
        }
        let Some((_, indices)) = matcher.fuzzy_indices(word, term) else {
            continue;
        };
        if indices.len() != term_len {
            continue;
        }
        let span = indices[term_len - 1] - indices[0] + 1;
        if span > max_span || indices.windows(2).any(|pair| pair[1] - pair[0] > 3) {
            continue;
        }
        let skipped = (span - term_len) as u32;
        best_skips = Some(best_skips.map_or(skipped, |best| best.min(skipped)));
    }

    best_skips
}

#[cfg(test)]
mod tests {
    use super::*;

    fn score(doc: &SearchDocument, query: &str) -> Option<ScoredMatch> {
        let matcher = SkimMatcherV2::default();
        let prepared = PreparedQuery::new(query)?;
        score_document(doc, &prepared, &matcher)
    }

    #[test]
    fn test_normalization_strips_accents_and_collapses_whitespace() {
        assert_eq!(normalize_search_text("  Café\t\nCrème  "), "cafe creme");
        assert_eq!(normalize_search_text("Cafe\u{0301}"), "cafe");
    }

    #[test]
    fn test_exact_match_beats_partial_title_match() {
        let exact = SearchDocument::new().with_field(SearchFieldKind::Title, "Firefox");
        let partial =
            SearchDocument::new().with_field(SearchFieldKind::Title, "Firefox troubleshooting");

        let exact_score = score(&exact, "firefox").expect("exact match");
        let partial_score = score(&partial, "firefox").expect("partial match");

        assert_eq!(exact_score.quality, MatchQuality::ExactPrimary);
        assert_eq!(partial_score.quality, MatchQuality::PrimaryWordPrefix);
        assert!(exact_score.sort_key() < partial_score.sort_key());
    }

    #[test]
    fn test_executable_basename_matches_primary_fields() {
        let config = RaffiConfig {
            description: Some("Visual Studio Code".to_string()),
            binary: Some("/usr/bin/code".to_string()),
            ..Default::default()
        };
        let doc = SearchDocument::from_launcher(&config);
        let matched = score(&doc, "code").expect("code matches executable basename");
        assert_eq!(matched.quality, MatchQuality::ExactPrimary);
        assert_eq!(matched.score, 10_000);
    }

    #[test]
    fn test_launcher_does_not_index_arguments_script_or_alias() {
        let config = RaffiConfig {
            description: Some("Editor".into()),
            binary: Some("/usr/bin/code".into()),
            args: Some(vec!["secret-argument".into()]),
            script: Some("secret-script".into()),
            alias: Some("secret-alias".into()),
            ..Default::default()
        };
        let document = SearchDocument::from_launcher(&config);
        assert!(score(&document, "editor").is_some());
        assert!(score(&document, "code").is_some());
        for excluded in ["secret-argument", "secret-script", "secret-alias"] {
            assert!(score(&document, excluded).is_none(), "{excluded}");
        }
    }

    #[test]
    fn test_command_documents_preserve_provider_aliases_as_literal_metadata() {
        let audio = super::super::audio::fixture_items(true);
        for (alias, title) in [("louder", "Volume up"), ("quieter", "Volume down")] {
            let item = audio.iter().find(|item| item.title == title).unwrap();
            let document = SearchDocument::from_command_item(item);
            assert_eq!(
                score(&document, alias).unwrap().quality,
                MatchQuality::SecondaryOnly
            );
        }
        let slack = super::super::slack::account_rows(
            &super::super::slack::Live::default(),
            chrono::Local::now(),
        );
        let item = slack
            .iter()
            .find(|item| item.title == "Set yourself active")
            .unwrap();
        let document = SearchDocument::from_command_item(item);
        for alias in ["online", "available"] {
            assert_eq!(
                score(&document, alias).unwrap().quality,
                MatchQuality::SecondaryOnly
            );
        }
        assert!(score(&document, "onln").is_none(), "metadata stays literal");
    }

    #[test]
    fn test_command_documents_preserve_non_last_workspace_window_metadata() {
        let items = super::super::windows::niri_workspace_items(
            br#"[
                {"id": 1, "title": "Most recent", "app_id": "editor", "workspace_id": 1, "is_focused": true},
                {"id": 2, "title": "Hidden cookbook", "app_id": "recipe-browser", "workspace_id": 1}
            ]"#,
            br#"[{"id": 1, "idx": 1, "name": "Work"}]"#,
            4242,
        ).unwrap();
        let item = &items[0];
        assert!(!item.subtitle.contains("cookbook"));
        let document = SearchDocument::from_command_item(item);
        for query in ["cookbook", "recipe-browser"] {
            assert_eq!(
                score(&document, query).unwrap().quality,
                MatchQuality::SecondaryOnly
            );
        }
    }

    #[test]
    fn test_multiword_out_of_order_and_cross_field_matching() {
        let title_doc = SearchDocument::new()
            .with_field(SearchFieldKind::Title, "Rust Documentation")
            .with_field(SearchFieldKind::Keyword, "docs");
        let url_doc = SearchDocument::new()
            .with_field(SearchFieldKind::Title, "Release Notes")
            .with_field(
                SearchFieldKind::Metadata,
                "https://example.com/rust/docs/index.html",
            );
        let cross_doc = SearchDocument::new()
            .with_field(SearchFieldKind::Title, "Rust")
            .with_field(SearchFieldKind::Subtitle, "Official docs portal");

        let prefix_title = score(&title_doc, "doc rust").expect("out-of-order title prefix match");
        let keyword_score = score(&title_doc, "docs rust").expect("title + keyword match");
        let cross_score = score(&cross_doc, "docs rust").expect("cross-field subtitle match");
        let url_score = score(&url_doc, "docs rust").expect("metadata match");

        assert_eq!(prefix_title.quality, MatchQuality::PrimaryWordPrefix);
        assert_eq!(keyword_score.quality, MatchQuality::MixedFields);
        assert_eq!(cross_score.quality, MatchQuality::MixedFields);
        assert_eq!(url_score.quality, MatchQuality::SecondaryOnly);
        assert!(prefix_title.sort_key() < keyword_score.sort_key());
        assert!(keyword_score.sort_key() < cross_score.sort_key());
        assert!(cross_score.sort_key() < url_score.sort_key());
    }

    #[test]
    fn test_abbreviation_matches_primary_but_not_word_prefix_tier() {
        let vsc = SearchDocument::new().with_field(SearchFieldKind::Title, "Visual Studio Code");
        let prefix = SearchDocument::new().with_field(SearchFieldKind::Title, "Vscodium");

        let vsc_score = score(&vsc, "vsc").expect("abbreviation match");
        let prefix_score = score(&prefix, "vsc").expect("prefix match");

        assert_eq!(vsc_score.quality, MatchQuality::PrimaryMatch);
        assert_eq!(prefix_score.quality, MatchQuality::PrimaryWordPrefix);
        assert!(prefix_score.sort_key() < vsc_score.sort_key());
    }

    #[test]
    fn test_compact_subsequence_accepted_in_primary_rejected_in_secondary() {
        let primary = SearchDocument::new().with_field(SearchFieldKind::Title, "Alacritty");
        let secondary = SearchDocument::new()
            .with_field(SearchFieldKind::Title, "Terminal")
            .with_field(SearchFieldKind::Subtitle, "Alacritty");
        let scattered = SearchDocument::new()
            .with_field(SearchFieldKind::Title, "Alpha Beta Gamma")
            .with_field(
                SearchFieldKind::Metadata,
                "https://example.com/a/very/long/path/with/b/and/g",
            );

        let compact = score(&primary, "alcr").expect("compact primary subsequence");
        assert_eq!(compact.quality, MatchQuality::PrimaryMatch);
        assert!(score(&secondary, "alcr").is_none());
        assert_eq!(
            score(&scattered, "abg").expect("abbreviation").quality,
            MatchQuality::PrimaryMatch
        );
        assert!(score(&scattered, "axg").is_none());
    }

    #[test]
    fn test_compact_subsequence_uses_character_positions() {
        for (title, query) in [
            ("终端模拟器", "终模器"),
            ("øabc", "øac"),
            ("界axbyc", "abc"),
        ] {
            let doc = SearchDocument::new().with_field(SearchFieldKind::Title, title);
            assert_eq!(
                score(&doc, query).expect("compact Unicode match").quality,
                MatchQuality::PrimaryMatch,
                "{title}: {query}"
            );
        }
    }

    #[test]
    fn test_every_term_determines_category_coverage() {
        let doc = SearchDocument::new()
            .with_field(SearchFieldKind::Title, "Visual Studio Code")
            .with_field(SearchFieldKind::Subtitle, "Remote SSH workspace");

        let abbr_and_prefix = score(&doc, "vsc studio").expect("vsc + studio");
        assert_eq!(abbr_and_prefix.quality, MatchQuality::PrimaryMatch);

        let primary_and_secondary = score(&doc, "code ssh").expect("code + ssh");
        assert_eq!(primary_and_secondary.quality, MatchQuality::MixedFields);
    }

    #[test]
    fn test_duplicate_fields_do_not_amplify_scores() {
        let single = SearchDocument::new().with_field(SearchFieldKind::Title, "Firefox");
        let duplicated = SearchDocument::new()
            .with_field(SearchFieldKind::Title, "Firefox")
            .with_field(SearchFieldKind::Title, "Firefox")
            .with_field(SearchFieldKind::Executable, "firefox")
            .with_field(SearchFieldKind::Subtitle, "Firefox");

        assert_eq!(score(&single, "fire"), score(&duplicated, "fire"));
    }

    #[test]
    fn test_accent_normalization_and_repeated_terms() {
        let doc = SearchDocument::new().with_field(SearchFieldKind::Title, "Café Crème");
        let composed = score(&doc, "cafe").expect("cafe matches Café");
        let decomposed = score(&doc, "cafe\u{0301}").expect("decomposed query matches Café");
        let repeated = score(&doc, "cafe   cafe").expect("repeated terms deduplicated");

        assert_eq!(composed, decomposed);
        assert_eq!(composed, repeated);
    }
}
