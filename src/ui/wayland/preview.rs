use std::fs::{self, File};
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use chrono::{DateTime, Local};

use super::browser::read_directory;
use super::clipboard::{ClipItem, ItemKind};
use super::command_list::{CommandItem, CommandListState};
use super::devtools::DevRow;
use super::file_preview::FilePreviewOutput;
use super::help::HelpEntry;
use super::main_view::{Block as MainViewBlock, Source as MainViewSource};
use super::snippets::SnippetIcon;
use super::state::{
    CurrencyConversion, CurrencyResult, EmojiEntry, FallbackAction, FileBrowserEntry,
    KeywordSuggestion, LauncherApp, MultiCurrencyResult, ResolvedFallback, WebSearchActiveState,
};
use super::types::{
    CalculatorResult, PreparedPreview, PreparedPreviewBody, ScriptFilterItem, ScriptFilterPreview,
    ScriptFilterResult,
};
use crate::{AddonsConfig, ClipboardAction, RaffiConfig, TextSnippet};

/// Maximum number of bytes read from a file when generating a text preview.
pub(super) const MAX_FILE_PREVIEW_BYTES: usize = 8 * 1024;
/// Maximum number of lines kept in a text file preview.
pub(super) const MAX_FILE_PREVIEW_LINES: usize = 100;
/// Maximum number of directory entries shown in a directory preview.
pub(super) const MAX_DIR_PREVIEW_ENTRIES: usize = 25;

/// Icon or hero visual for the preview header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PreviewIcon {
    File(String),
    Glyph(String),
}

/// Main body block in the right-hand preview pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PreviewBody {
    /// Multiline plain or monospace text block, with an optional section heading.
    Text {
        heading: Option<String>,
        content: String,
        truncated: bool,
    },
    /// ANSI-styled multiline text block (for Script Filter output), with an optional heading.
    AnsiText {
        heading: Option<String>,
        content: String,
    },
    /// Scaled image or SVG preview (for File Browser images or QuickLook image paths).
    Image { path: String },
    /// Image at the pane width and its own height, up to half the pane
    /// (for a browser tab's page image).
    Thumbnail { path: String },
    /// Scaled image decoded from memory (for clipboard history images).
    ImageData { handle: iced::widget::image::Handle },
    /// Giant centered glyph/emoji hero display.
    HeroGlyph { glyph: String },
    /// Large centered value display (for Calculator and Currency Converter).
    HeroValue {
        expression: Option<String>,
        value: String,
    },
    /// Directory listing summary for a folder in the File Browser.
    DirectoryList {
        entries: Vec<(String, bool)>,
        total_count: usize,
    },
}

/// Complete structured model for the right-hand preview pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PreviewData {
    pub title: String,
    pub subtitle: Option<String>,
    pub badge: Option<String>,
    pub icon: Option<PreviewIcon>,
    pub bodies: Vec<PreviewBody>,
    pub metadata: Vec<(String, String)>,
}

/// Cached inspection of a local filesystem path for the File Browser or Script Filter QuickLook.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InspectedFile {
    pub path: String,
    pub show_hidden: bool,
    pub bodies: Vec<PreviewBody>,
    pub metadata: Vec<(String, String)>,
}

/// Check if an extension corresponds to a previewable raster or vector image.
pub(super) fn is_previewable_image_path(path: &Path) -> bool {
    matches!(
        image_mime_type(path),
        Some(
            "image/png"
                | "image/jpeg"
                | "image/gif"
                | "image/webp"
                | "image/bmp"
                | "image/vnd.microsoft.icon"
                | "image/svg+xml"
        )
    )
}

pub(super) fn image_mime_type(path: &Path) -> Option<&'static str> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())?
        .to_ascii_lowercase();
    match ext.as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        "bmp" => Some("image/bmp"),
        "ico" => Some("image/vnd.microsoft.icon"),
        "svg" => Some("image/svg+xml"),
        "tif" | "tiff" => Some("image/tiff"),
        _ => None,
    }
}

/// Format a byte count into a human-readable string (`B`, `KiB`, `MiB`, `GiB`).
pub(super) fn format_byte_size(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;

    if bytes < 1024 {
        format!("{bytes} B")
    } else if (bytes as f64) < MIB {
        format!("{:.1} KiB", bytes as f64 / KIB)
    } else if (bytes as f64) < GIB {
        format!("{:.1} MiB", bytes as f64 / MIB)
    } else {
        format!("{:.2} GiB", bytes as f64 / GIB)
    }
}

/// Format a Unix mode bitmask into an `rwxr-xr-x (0755)` string.
fn format_permissions(mode: u32) -> String {
    let flags = [
        (0o400, 'r'),
        (0o200, 'w'),
        (0o100, 'x'),
        (0o040, 'r'),
        (0o020, 'w'),
        (0o010, 'x'),
        (0o004, 'r'),
        (0o002, 'w'),
        (0o001, 'x'),
    ];
    let mut rwx = String::with_capacity(9);
    for (bit, ch) in flags {
        if mode & bit != 0 {
            rwx.push(ch);
        } else {
            rwx.push('-');
        }
    }
    format!("{rwx} ({:04o})", mode & 0o7777)
}

/// Inspect a filesystem path and produce bounded preview bodies and metadata.
pub(super) fn inspect_path(full_path: &str, show_hidden: bool) -> InspectedFile {
    let path = Path::new(full_path);
    let mut bodies = Vec::new();
    let mut metadata = Vec::new();

    if let Ok(symlink_meta) = fs::symlink_metadata(path) {
        if symlink_meta.file_type().is_symlink() {
            if let Ok(target) = fs::read_link(path) {
                metadata.push(("Symlink".to_string(), target.to_string_lossy().to_string()));
            }
        }
    }

    let meta = match fs::metadata(path) {
        Ok(m) => m,
        Err(err) => {
            bodies.push(PreviewBody::Text {
                heading: Some("Error".to_string()),
                content: format!("Cannot inspect path: {err}"),
                truncated: false,
            });
            return InspectedFile {
                path: full_path.to_string(),
                show_hidden,
                bodies,
                metadata,
            };
        }
    };

    if meta.is_dir() {
        metadata.push(("Type".to_string(), "Directory".to_string()));
        let entries = read_directory(full_path, show_hidden, crate::FileBrowserSort::Name, false);
        let total_count = entries.len();
        let preview_entries: Vec<(String, bool)> = entries
            .into_iter()
            .take(MAX_DIR_PREVIEW_ENTRIES)
            .map(|e| (e.name, e.is_dir))
            .collect();
        metadata.push(("Items".to_string(), total_count.to_string()));
        bodies.push(PreviewBody::DirectoryList {
            entries: preview_entries,
            total_count,
        });
    } else {
        metadata.push(("Size".to_string(), format_byte_size(meta.len())));
        if !meta.is_file() {
            metadata.push(("Type".to_string(), "Special file".to_string()));
            bodies.push(PreviewBody::Text {
                heading: Some("Preview unavailable".to_string()),
                content: "Special files cannot be previewed.".to_string(),
                truncated: false,
            });
        } else if is_previewable_image_path(path) {
            metadata.push(("Type".to_string(), "Image".to_string()));
            bodies.push(PreviewBody::Image {
                path: full_path.to_string(),
            });
        } else if let Ok(file) = File::open(path) {
            let mut buf = Vec::with_capacity(MAX_FILE_PREVIEW_BYTES.min(meta.len() as usize) + 1);
            let read_res = file
                .take((MAX_FILE_PREVIEW_BYTES + 1) as u64)
                .read_to_end(&mut buf);
            if read_res.is_ok() {
                if buf.is_empty() {
                    metadata.push(("Type".to_string(), "Empty file".to_string()));
                    bodies.push(PreviewBody::Text {
                        heading: Some("Contents".to_string()),
                        content: "(Empty file)".to_string(),
                        truncated: false,
                    });
                } else {
                    let byte_truncated = buf.len() > MAX_FILE_PREVIEW_BYTES;
                    let slice = if byte_truncated {
                        &buf[..MAX_FILE_PREVIEW_BYTES]
                    } else {
                        &buf[..]
                    };

                    if slice.contains(&0) {
                        metadata.push(("Type".to_string(), "Binary file".to_string()));
                        bodies.push(PreviewBody::Text {
                            heading: Some("Binary File".to_string()),
                            content: "Binary file preview is not shown.".to_string(),
                            truncated: false,
                        });
                    } else {
                        // Trim incomplete trailing UTF-8 sequence if truncated mid-codepoint.
                        let text_str = match std::str::from_utf8(slice) {
                            Ok(s) => Some(s.to_string()),
                            Err(err) if byte_truncated && err.error_len().is_none() => {
                                let valid = &slice[..err.valid_up_to()];
                                std::str::from_utf8(valid).ok().map(str::to_string)
                            }
                            Err(_) => None,
                        };

                        if let Some(text_content) = text_str {
                            metadata.push(("Type".to_string(), "Text file".to_string()));
                            let mut lines: Vec<&str> = text_content.lines().collect();
                            let line_truncated = lines.len() > MAX_FILE_PREVIEW_LINES;
                            if line_truncated {
                                lines.truncate(MAX_FILE_PREVIEW_LINES);
                            }
                            bodies.push(PreviewBody::Text {
                                heading: Some("Contents".to_string()),
                                content: lines.join("\n"),
                                truncated: byte_truncated || line_truncated,
                            });
                        } else {
                            metadata.push(("Type".to_string(), "Binary file".to_string()));
                            bodies.push(PreviewBody::Text {
                                heading: Some("Binary File".to_string()),
                                content: "Binary file preview is not shown.".to_string(),
                                truncated: false,
                            });
                        }
                    }
                }
            }
        }
    }

    if let Ok(modified) = meta.modified() {
        let dt: DateTime<Local> = modified.into();
        metadata.push((
            "Modified".to_string(),
            dt.format("%Y-%m-%d %H:%M").to_string(),
        ));
    }

    metadata.push((
        "Permissions".to_string(),
        format_permissions(meta.permissions().mode()),
    ));

    InspectedFile {
        path: full_path.to_string(),
        show_hidden,
        bodies,
        metadata,
    }
}

/// Resolve a potential local file path from a string or `file://` URL.
fn resolve_local_preview_path(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.contains('\n') {
        return None;
    }
    let (path_str, is_file_url) = if let Some(url_path) = trimmed.strip_prefix("file://") {
        let path_start = url_path.find('/')?;
        let authority = &url_path[..path_start];
        if !authority.is_empty() && !authority.eq_ignore_ascii_case("localhost") {
            return None;
        }
        (&url_path[path_start..], true)
    } else {
        (trimmed, false)
    };
    if !(path_str.starts_with('/') || path_str.starts_with("~/")) {
        return None;
    }
    let expanded = if is_file_url {
        crate::expand_config_value(&percent_decode_path(path_str)?)
    } else {
        crate::expand_config_value(path_str)
    };
    if Path::new(&expanded).exists() {
        Some(expanded)
    } else {
        None
    }
}

pub(super) fn percent_decode_path(path: &str) -> Option<String> {
    let bytes = path.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;

    while index < bytes.len() {
        if bytes[index] == b'%' {
            let escape = std::str::from_utf8(bytes.get(index + 1..index + 3)?).ok()?;
            decoded.push(u8::from_str_radix(escape, 16).ok()?);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }

    String::from_utf8(decoded).ok()
}

fn describe_action(action: &str) -> String {
    match action {
        "copy" => "Copy to clipboard".to_string(),
        "insert" => "Type into focused app".to_string(),
        "hypr_paste" => "Paste into original window (Hyprland)".to_string(),
        custom => format!("Run `{custom}`"),
    }
}

fn describe_clipboard_action(action: ClipboardAction) -> String {
    match action {
        ClipboardAction::Copy => "Copy with all formats".to_string(),
        ClipboardAction::HyprPaste => describe_action("hypr_paste"),
    }
}

pub(super) fn describe_file_action(action: &str) -> String {
    match action {
        "open" => "Open with xdg-open".to_string(),
        "copy" => "Copy full path".to_string(),
        "insert" => "Type full path into focused app".to_string(),
        custom => describe_action(custom),
    }
}

pub(super) fn calculator_preview(calc: &CalculatorResult) -> PreviewData {
    let result_str = super::support::format_calculator_value(calc.result);

    let mut metadata = vec![
        ("Expression".to_string(), calc.expression.clone()),
        ("Decimal".to_string(), result_str.clone()),
        ("Scientific".to_string(), format!("{:e}", calc.result)),
    ];

    if calc.result.fract() == 0.0
        && calc.result >= (i64::MIN as f64)
        && calc.result <= (i64::MAX as f64)
    {
        let int_val = calc.result as i64;
        metadata.push(("Hexadecimal".to_string(), format!("0x{int_val:X}")));
        metadata.push(("Octal".to_string(), format!("0o{int_val:o}")));
        metadata.push(("Binary".to_string(), format!("0b{int_val:b}")));
    }

    metadata.push((
        "Enter".to_string(),
        format!("Copy {result_str} to clipboard"),
    ));

    PreviewData {
        title: "Calculator".to_string(),
        subtitle: None,
        badge: Some("Math".to_string()),
        icon: None,
        bodies: vec![PreviewBody::HeroValue {
            expression: Some(calc.expression.clone()),
            value: result_str,
        }],
        metadata,
    }
}

impl LauncherApp {
    /// Refresh the cached file preview whenever the selected File Browser item or
    /// Script Filter QuickLook file changes.
    pub(super) fn refresh_file_preview_cache(&mut self) {
        if !self.view.show_preview {
            self.file_browser.cached_preview = None;
            return;
        }
        if let Some((path, show_hidden)) = self.selected_file_to_inspect() {
            let needs_update = self
                .file_browser
                .cached_preview
                .as_ref()
                .map(|c| c.path != path || c.show_hidden != show_hidden)
                .unwrap_or(true);
            if needs_update {
                self.file_browser.cached_preview = Some(inspect_path(&path, show_hidden));
            }
        }
    }

    /// Return `(path, show_hidden)` if the currently selected item references a local file
    /// that should be inspected for previewing.
    pub(super) fn selected_file_to_inspect(&self) -> Option<(String, bool)> {
        if self.view.help_active {
            return None;
        }
        let mut current_index = self.keyword_rows();

        if self.script_filter.loading || self.script_filter.help_message.is_some() {
            current_index += 1;
        } else if let Some(sf_result) = &self.script_filter.results {
            let count = sf_result.items.len();
            if self.selected_index >= current_index && self.selected_index < current_index + count {
                let index = self.selected_index - current_index;
                let item = &sf_result.items[index];
                let preview = item.preview.as_ref();
                let file = self
                    .script_filter
                    .previews
                    .get(&index)
                    .and_then(|dynamic| dynamic.preview.as_ref())
                    .or_else(|| preview.and_then(ScriptFilterPreview::as_rich))
                    .and_then(PreparedPreview::file_path)
                    .or_else(|| preview.and_then(ScriptFilterPreview::as_text));
                if let Some(local) = file.and_then(resolve_local_preview_path) {
                    return Some((local, false));
                }
                if let Some(ql) = &item.quicklookurl {
                    if let Some(local) = resolve_local_preview_path(ql) {
                        return Some((local, false));
                    }
                }
                return None;
            }
            current_index += count;
        }

        if self.text_snippets.loading {
            current_index += 1;
        } else if self.text_snippets.active {
            current_index += self.text_snippets.filtered.len();
        }

        if self.emoji.active {
            current_index += self.emoji.filtered.len();
        }

        current_index += self.clipboard.row_count();

        current_index += self.command_list.row_count();

        if self.devtools.active {
            current_index += self.devtools.rows.len();
        }

        if self.file_browser.active {
            let count = self.file_browser.entries.len();
            if self.selected_index >= current_index && self.selected_index < current_index + count {
                let entry = &self.file_browser.entries[self.selected_index - current_index];
                return Some((entry.full_path.clone(), self.file_browser.show_hidden));
            }
        }

        None
    }

    /// Resolve the preview data for the currently selected item across all modes.
    pub(super) fn resolve_selected_preview(&self) -> Option<PreviewData> {
        if !self.view.show_preview {
            return None;
        }

        if self.view.help_active {
            let &entry_idx = self.help_filtered.get(self.selected_index)?;
            let entry = self.help_entries.get(entry_idx)?;
            return Some(self.build_help_preview(entry));
        }

        let index = self.selected_index;
        let mut current_index = 0;

        // 1. Keyword suggestions
        if let Some(suggestion) = self.keyword_at(index) {
            return Some(self.build_keyword_suggestion_preview(suggestion));
        }
        current_index += self.keyword_rows();

        // 2. Script Filter
        if self.script_filter.loading {
            if index == current_index {
                let name = self
                    .script_filter
                    .loading_name
                    .as_deref()
                    .unwrap_or("Script Filter");
                return Some(PreviewData {
                    title: name.to_string(),
                    subtitle: Some("Running external command...".to_string()),
                    badge: Some("Script Filter".to_string()),
                    icon: None,
                    bodies: vec![PreviewBody::Text {
                        heading: None,
                        content: format!("Waiting for {name} output..."),
                        truncated: false,
                    }],
                    metadata: Vec::new(),
                });
            }
            current_index += 1;
        } else if let Some(help_msg) = &self.script_filter.help_message {
            if index == current_index {
                return Some(PreviewData {
                    title: "Script Filter".to_string(),
                    subtitle: None,
                    badge: Some("Input required".to_string()),
                    icon: None,
                    bodies: vec![PreviewBody::Text {
                        heading: None,
                        content: help_msg.clone(),
                        truncated: false,
                    }],
                    metadata: Vec::new(),
                });
            }
            current_index += 1;
        } else if let Some(sf_result) = &self.script_filter.results {
            let count = sf_result.items.len();
            if index >= current_index && index < current_index + count {
                let item_index = index - current_index;
                return Some(self.build_script_filter_preview(
                    item_index,
                    &sf_result.items[item_index],
                    sf_result,
                    self.script_filter.action.as_deref(),
                    self.script_filter.secondary_action.as_deref(),
                    true,
                ));
            }
            current_index += count;
        }

        // 3. Text Snippets
        if self.text_snippets.loading {
            if index == current_index {
                return Some(PreviewData {
                    title: "Text Snippets".to_string(),
                    subtitle: Some("Loading snippets...".to_string()),
                    badge: Some("Snippets".to_string()),
                    icon: None,
                    bodies: Vec::new(),
                    metadata: Vec::new(),
                });
            }
            current_index += 1;
        } else if self.text_snippets.active {
            let count = self.text_snippets.filtered.len();
            if count > 0 && index >= current_index && index < current_index + count {
                let snippet_idx = self.text_snippets.filtered[index - current_index];
                if let Some(snippet) = self.text_snippets.items.get(snippet_idx) {
                    return Some(self.build_text_snippet_preview(snippet_idx, snippet));
                }
            }
            current_index += count;
        }

        // 4. Emoji Picker
        if self.emoji.active {
            let count = self.emoji.filtered.len();
            if count > 0 && index >= current_index && index < current_index + count {
                let emoji_idx = self.emoji.filtered[index - current_index];
                if let Some(entry) = self.emoji.data.get(emoji_idx) {
                    return Some(self.build_emoji_preview(entry));
                }
            }
            current_index += count;
        }

        // 4b. Clipboard history
        if self.clipboard.active {
            let count = self.clipboard.row_count();
            if index >= current_index && index < current_index + count {
                return Some(match self.clipboard.items.get(index - current_index) {
                    Some(item) => self.build_clipboard_preview(item),
                    None => PreviewData {
                        title: "Clipboard history".to_string(),
                        subtitle: Some(self.clipboard.status()),
                        badge: Some("Clipboard".to_string()),
                        icon: None,
                        bodies: Vec::new(),
                        metadata: Vec::new(),
                    },
                });
            }
            current_index += count;
        }

        // 4c. Command lists (system commands, windows, media, audio, kill,
        // Bluetooth)
        if self.command_list.is_active() {
            let count = self.command_list.row_count();
            if index >= current_index && index < current_index + count {
                return self
                    .command_list
                    .item(index - current_index)
                    .map(|item| self.build_command_list_preview(&self.command_list, item));
            }
            current_index += count;
        }

        // 4d. Developer tools
        if self.devtools.active {
            let count = self.devtools.rows.len();
            if index >= current_index && index < current_index + count {
                return Some(devtools_preview(&self.devtools.rows[index - current_index]));
            }
            current_index += count;
        }

        // 5. File Browser
        if self.file_browser.active {
            let count = self.file_browser.entries.len();
            if count > 0 && index >= current_index && index < current_index + count {
                let entry = &self.file_browser.entries[index - current_index];
                return Some(self.build_file_browser_preview(entry));
            }
            current_index += count;
        }

        // 6. Web Search
        if let Some(ws) = &self.web_search.active {
            if index == current_index {
                return Some(self.build_web_search_preview(ws));
            }
            current_index += 1;
        }

        // 7. Currency Converter
        if self.currency.help {
            if index == current_index {
                return Some(self.build_currency_help_preview());
            }
            current_index += 1;
        }
        if self.currency.loading {
            if index == current_index {
                return Some(PreviewData {
                    title: "Currency Converter".to_string(),
                    subtitle: Some("Fetching live exchange rate...".to_string()),
                    badge: Some("Currency".to_string()),
                    icon: None,
                    bodies: Vec::new(),
                    metadata: Vec::new(),
                });
            }
            current_index += 1;
        } else if let Some(currency_res) = &self.currency.result {
            if index == current_index {
                return Some(self.build_currency_single_preview(currency_res));
            }
            current_index += 1;
        }

        if self.currency.multi_loading {
            if index == current_index {
                return Some(PreviewData {
                    title: "Currency Converter".to_string(),
                    subtitle: Some("Fetching live exchange rates...".to_string()),
                    badge: Some("Currency".to_string()),
                    icon: None,
                    bodies: Vec::new(),
                    metadata: Vec::new(),
                });
            }
            current_index += 1;
        } else if let Some(multi_res) = &self.currency.multi_result {
            let count = multi_res.conversions.len();
            if index >= current_index && index < current_index + count {
                let conv = &multi_res.conversions[index - current_index];
                return Some(self.build_currency_multi_preview(multi_res, conv));
            }
            current_index += count;
        }

        // 8. Inline results (calculator)
        if let Some(inline) = &self.inline_result {
            let count = inline.rows.len();
            if index >= current_index && index < current_index + count {
                return inline.rows[index - current_index].preview.clone();
            }
            current_index += count;
        }

        if let Some((source, row)) = self.selected_main_view() {
            return self.build_main_view_preview(source, row);
        }
        current_index += self.main_view.row_count(MainViewBlock::BeforeLaunchers);

        // 9. Standard Launcher configs (disabled by default unless explicitly enabled
        // via `general.show_preview: true`, launcher `show_preview: true`, or toggled with Ctrl+P)
        let config_offset = index.saturating_sub(current_index);
        if config_offset < self.filtered_configs.len() {
            let real_config_idx = self.filtered_configs[config_offset];
            if let Some(config) = self.configs.get(real_config_idx) {
                let enabled = match config.show_preview {
                    Some(explicit) => explicit,
                    None => self.view.show_launcher_preview,
                };
                if enabled {
                    return Some(self.build_launcher_preview(real_config_idx, config));
                }
                return None;
            }
        }
        current_index += self.filtered_configs.len();
        current_index += self.main_view.row_count(MainViewBlock::AfterLaunchers);

        // 10. Fallbacks
        let fallback_offset = index.saturating_sub(current_index);
        if fallback_offset < self.fallback_count() {
            if let Some(fallback) = self.fallbacks.get(fallback_offset) {
                return Some(self.build_fallback_preview(fallback));
            }
        }

        None
    }

    fn resolve_named_icon(&self, icon_name: &str) -> Option<PreviewIcon> {
        if self.no_icons {
            return None;
        }
        crate::resolve_icon_reference(icon_name, &self.icon_map)
            .filter(|p| Path::new(p).is_file())
            .map(PreviewIcon::File)
    }

    fn keyword_preview_icon(&self, icon_name: &str) -> Option<PreviewIcon> {
        self.keyword_icon(icon_name).map(|icon| match icon {
            SnippetIcon::File(path) => PreviewIcon::File(path),
            SnippetIcon::Glyph(glyph) => PreviewIcon::Glyph(glyph),
        })
    }

    fn build_text_snippet_preview(&self, snippet_idx: usize, snippet: &TextSnippet) -> PreviewData {
        let expanded = self
            .text_snippets
            .previews
            .get(snippet_idx)
            .and_then(Option::as_deref);
        self.snippet_preview(
            snippet,
            self.snippet_icon(snippet),
            expanded,
            self.text_snippets.action.as_deref(),
            self.text_snippets.secondary_action.as_deref(),
        )
    }

    /// Preview of `snippet`, showing `expanded`, its placeholders filled in,
    /// when known.
    fn snippet_preview(
        &self,
        snippet: &TextSnippet,
        icon: Option<SnippetIcon>,
        expanded: Option<&str>,
        action: Option<&str>,
        secondary_action: Option<&str>,
    ) -> PreviewData {
        let icon = icon.map(|si| match si {
            SnippetIcon::File(path) => PreviewIcon::File(path),
            SnippetIcon::Glyph(glyph) => PreviewIcon::Glyph(glyph),
        });
        let expanded_value = expanded.unwrap_or(&snippet.value);

        let mut bodies = vec![PreviewBody::Text {
            heading: Some("Snippet Content".to_string()),
            content: expanded_value.to_string(),
            truncated: false,
        }];

        // Expansions are only computed when placeholders are enabled.
        if expanded_value != snippet.value {
            bodies.push(PreviewBody::Text {
                heading: Some("Raw Template".to_string()),
                content: snippet.value.clone(),
                truncated: false,
            });
        }

        let mut metadata = Vec::new();
        if let Some(kw) = &snippet.keyword {
            metadata.push(("Keyword".to_string(), kw.clone()));
        }
        metadata.push((
            "Length".to_string(),
            format!(
                "{} chars, {} lines",
                expanded_value.chars().count(),
                expanded_value.lines().count().max(1)
            ),
        ));
        if snippet.value.contains("{cursor}") {
            metadata.push((
                "Cursor".to_string(),
                "Positions caret at {cursor}".to_string(),
            ));
        }

        let primary_action = action.unwrap_or("copy");
        let secondary_action = secondary_action.unwrap_or("insert");
        metadata.push(("Enter".to_string(), describe_action(primary_action)));
        metadata.push(("Ctrl+Enter".to_string(), describe_action(secondary_action)));

        PreviewData {
            title: snippet.name.clone(),
            subtitle: None,
            badge: snippet
                .keyword
                .clone()
                .or_else(|| Some("Snippet".to_string())),
            icon,
            bodies,
            metadata,
        }
    }

    fn build_file_browser_preview(&self, entry: &FileBrowserEntry) -> PreviewData {
        let icon = if self.no_icons {
            None
        } else {
            let icon_name = if entry.is_dir {
                "folder"
            } else {
                super::browser::mimetype_icon_name(&entry.full_path)
            };
            self.icon_map
                .get(icon_name)
                .filter(|p| Path::new(p).exists())
                .cloned()
                .map(PreviewIcon::File)
        };

        let inspected = self
            .file_browser
            .cached_preview
            .as_ref()
            .filter(|c| c.path == entry.full_path && c.show_hidden == self.file_browser.show_hidden)
            .cloned()
            .unwrap_or_else(|| inspect_path(&entry.full_path, self.file_browser.show_hidden));

        let config = &self.addons.file_browser;
        let mut metadata = vec![("Path".to_string(), entry.full_path.clone())];
        metadata.extend(inspected.metadata);
        metadata.push((
            "Enter".to_string(),
            if entry.is_dir {
                "Open directory".to_string()
            } else {
                describe_file_action(config.action())
            },
        ));
        metadata.push((
            "Ctrl+Enter".to_string(),
            describe_file_action(config.secondary_action()),
        ));
        metadata.push(("Tab".to_string(), "Autocomplete path".to_string()));
        metadata.push(("Ctrl+O".to_string(), "Open with xdg-open".to_string()));
        metadata.push(("Ctrl+B".to_string(), "Show in file manager".to_string()));
        metadata.push(("Ctrl+Y".to_string(), "Copy path".to_string()));
        if !entry.is_dir && image_mime_type(Path::new(&entry.full_path)).is_some() {
            metadata.push(("Ctrl+Alt+Y".to_string(), "Copy image".to_string()));
        }

        let mut data = PreviewData {
            title: if entry.is_dir {
                format!("{}/", entry.name)
            } else {
                entry.name.clone()
            },
            subtitle: None,
            badge: Some(if entry.is_dir { "Directory" } else { "File" }.to_string()),
            icon,
            bodies: inspected.bodies,
            metadata,
        };

        // The preview command's output replaces the built-in body once it
        // has answered; until then the built-in preview stays on screen.
        match self.file_browser.script_preview(&entry.full_path) {
            Some(Ok(Some(FilePreviewOutput::Text(content)))) => {
                data.bodies = vec![PreviewBody::AnsiText {
                    heading: None,
                    content: content.clone(),
                }];
            }
            Some(Ok(Some(FilePreviewOutput::Rich(preview)))) => {
                self.apply_prepared_preview(&mut data, preview);
            }
            Some(Err(error)) => {
                data.metadata
                    .insert(1, ("Preview".to_string(), error.clone()));
            }
            Some(Ok(None)) | None => {}
        }
        data
    }

    fn build_script_filter_preview(
        &self,
        index: usize,
        item: &ScriptFilterItem,
        sf_result: &ScriptFilterResult,
        action: Option<&str>,
        secondary_action: Option<&str>,
        dynamic: bool,
    ) -> PreviewData {
        let icon_path = if self.no_icons {
            None
        } else {
            item.icon
                .as_ref()
                .and_then(|icon| icon.path.as_deref())
                .map(crate::expand_config_value)
                .filter(|p| Path::new(p).exists())
                .or_else(|| {
                    sf_result
                        .default_icon
                        .as_deref()
                        .and_then(|name| self.icon_map.get(name).cloned())
                        .filter(|p| Path::new(p).exists())
                })
        };

        let mut bodies = Vec::new();
        let mut metadata = Vec::new();

        let static_text = item.preview.as_ref().and_then(ScriptFilterPreview::as_text);

        // Check local file preview from `preview` or `quicklookurl`
        let local_file = static_text
            .and_then(resolve_local_preview_path)
            .or_else(|| {
                item.quicklookurl
                    .as_deref()
                    .and_then(resolve_local_preview_path)
            });

        if let Some(path) = local_file {
            let inspected = self
                .file_browser
                .cached_preview
                .as_ref()
                .filter(|c| c.path == path)
                .cloned()
                .unwrap_or_else(|| inspect_path(&path, false));
            bodies.extend(inspected.bodies);
            metadata.extend(inspected.metadata);
        } else if let Some(preview_text) = static_text {
            bodies.push(PreviewBody::AnsiText {
                heading: Some("Preview".to_string()),
                content: preview_text.to_string(),
            });
        } else if let Some(largetype) = item.text.as_ref().and_then(|t| t.largetype.as_ref()) {
            bodies.push(PreviewBody::AnsiText {
                heading: Some("Details".to_string()),
                content: largetype.clone(),
            });
        } else if let Some(subtitle) = &item.subtitle {
            bodies.push(PreviewBody::AnsiText {
                heading: Some("Details".to_string()),
                content: subtitle.clone(),
            });
        }

        if let Some(arg) = &item.arg {
            metadata.push(("Argument".to_string(), arg.clone()));
        }
        if let Some(ql) = &item.quicklookurl {
            metadata.push(("QuickLook".to_string(), ql.clone()));
        }
        if let Some(copy_text) = item.text.as_ref().and_then(|t| t.copy.as_ref()) {
            metadata.push(("Copy Text".to_string(), copy_text.clone()));
        }

        if item.valid {
            let primary = action.unwrap_or("copy");
            metadata.push(("Enter".to_string(), describe_action(primary)));
            if let Some(secondary) = secondary_action {
                metadata.push(("Ctrl+Enter".to_string(), describe_action(secondary)));
            }
        }

        let mut data = PreviewData {
            title: super::ansi::strip_ansi(&item.title),
            subtitle: item
                .subtitle
                .as_deref()
                .map(super::ansi::strip_ansi)
                .filter(|_| item.preview.is_some() || item.text.is_some()),
            badge: Some("Script Filter".to_string()),
            icon: icon_path.map(PreviewIcon::File),
            bodies,
            metadata,
        };

        // A dynamic preview replaces the item's own rich preview.
        let dynamic = dynamic
            .then(|| self.script_filter.previews.get(&index))
            .flatten();
        if let Some(rich) = dynamic
            .and_then(|dynamic| dynamic.preview.as_ref())
            .or_else(|| item.preview.as_ref().and_then(ScriptFilterPreview::as_rich))
        {
            self.apply_prepared_preview(&mut data, rich);
        }
        if let Some(error) = dynamic.and_then(|dynamic| dynamic.error.as_ref()) {
            data.metadata.push(("Preview".to_string(), error.clone()));
        }
        data
    }

    /// Overlay a script-provided preview on the default Script Filter preview.
    fn apply_prepared_preview(&self, data: &mut PreviewData, preview: &PreparedPreview) {
        if let Some(title) = &preview.title {
            data.title = super::ansi::strip_ansi(title);
        }
        if let Some(subtitle) = &preview.subtitle {
            data.subtitle = Some(super::ansi::strip_ansi(subtitle));
        }
        if let Some(badge) = &preview.badge {
            data.badge = Some(badge.clone());
        }
        if let Some(icon) = &preview.icon {
            if let Some(icon) = self.resolve_icon_or_glyph(icon) {
                data.icon = Some(match icon {
                    SnippetIcon::File(path) => PreviewIcon::File(path),
                    SnippetIcon::Glyph(glyph) => PreviewIcon::Glyph(glyph),
                });
            }
        }

        let mut file_metadata = Vec::new();
        if preview.replaces_body {
            data.bodies.clear();
            let mut file_shown = false;
            for body in &preview.bodies {
                match body {
                    PreparedPreviewBody::Text { heading, content } => {
                        data.bodies.push(PreviewBody::AnsiText {
                            heading: heading.clone(),
                            content: content.clone(),
                        });
                    }
                    PreparedPreviewBody::Image(path) => {
                        let expanded = crate::expand_config_value(path);
                        if Path::new(&expanded).is_file() {
                            data.bodies.push(PreviewBody::Image { path: expanded });
                        }
                    }
                    PreparedPreviewBody::ImageData(handle) => {
                        data.bodies.push(PreviewBody::ImageData {
                            handle: handle.clone(),
                        });
                    }
                    // Only the first file is shown: it is the one kept in the
                    // inspection cache.
                    PreparedPreviewBody::File(path) if !file_shown => {
                        file_shown = true;
                        let Some(local) = resolve_local_preview_path(path) else {
                            continue;
                        };
                        let inspected = self
                            .file_browser
                            .cached_preview
                            .as_ref()
                            .filter(|c| c.path == local)
                            .cloned()
                            .unwrap_or_else(|| inspect_path(&local, false));
                        data.bodies.extend(inspected.bodies);
                        file_metadata = inspected.metadata;
                    }
                    PreparedPreviewBody::File(_) => {}
                }
            }
        }

        if !preview.metadata.is_empty() || !file_metadata.is_empty() {
            let mut metadata = preview.metadata.clone();
            metadata.extend(file_metadata);
            metadata.append(&mut data.metadata);
            data.metadata = metadata;
        }
    }

    fn build_launcher_preview(&self, config_idx: usize, config: &RaffiConfig) -> PreviewData {
        let title = config
            .description
            .clone()
            .unwrap_or_else(|| config.binary.clone().unwrap_or_default());

        let icon = if self.no_icons {
            None
        } else {
            self.launcher_icon_paths
                .get(config_idx)
                .and_then(Option::clone)
                .filter(|p| Path::new(p).exists())
                .map(PreviewIcon::File)
        };

        let mut bodies = Vec::new();
        let mut metadata = Vec::new();

        if let Some(script) = &config.script {
            let interpreter = config
                .binary
                .as_deref()
                .unwrap_or(&self.view.default_script_shell);
            bodies.push(PreviewBody::Text {
                heading: Some(format!("Script ({interpreter})")),
                content: script.trim_end().to_string(),
                truncated: false,
            });
            metadata.push(("Interpreter".to_string(), interpreter.to_string()));
        } else if let Some(binary) = &config.binary {
            let full_cmd = if let Some(args) = &config.args {
                if args.is_empty() {
                    binary.clone()
                } else {
                    format!("{} {}", binary, args.join(" "))
                }
            } else {
                binary.clone()
            };
            bodies.push(PreviewBody::Text {
                heading: Some("Command".to_string()),
                content: full_cmd,
                truncated: false,
            });
            metadata.push(("Binary".to_string(), binary.clone()));
        }

        if let Some(args) = &config.args {
            if !args.is_empty() {
                metadata.push(("Arguments".to_string(), args.join(" ")));
            }
        }

        if let Some(ifexist) = &config.ifexist {
            metadata.push(("Condition".to_string(), format!("ifexist: {ifexist}")));
        }
        if let Some(ifenvset) = &config.ifenvset {
            metadata.push(("Condition".to_string(), format!("ifenvset: {ifenvset}")));
        }
        if let Some(ifenvnotset) = &config.ifenvnotset {
            metadata.push((
                "Condition".to_string(),
                format!("ifenvnotset: {ifenvnotset}"),
            ));
        }
        if let Some(ifenveq) = &config.ifenveq {
            if ifenveq.len() == 2 {
                metadata.push((
                    "Condition".to_string(),
                    format!("${} == {}", ifenveq[0], ifenveq[1]),
                ));
            }
        }

        if let Some(mru) = self.mru_map.get(&title) {
            if mru.count > 0 {
                metadata.push(("Launches".to_string(), mru.count.to_string()));
            }
            if mru.last_used > 0 {
                if let Some(dt) = DateTime::from_timestamp(mru.last_used as i64, 0) {
                    let local_dt: DateTime<Local> = dt.into();
                    metadata.push((
                        "Last Used".to_string(),
                        local_dt.format("%Y-%m-%d %H:%M").to_string(),
                    ));
                }
            }
        }

        metadata.push((
            "Enter".to_string(),
            if config.script.is_some() {
                "Execute script".to_string()
            } else {
                "Launch application".to_string()
            },
        ));

        PreviewData {
            title,
            subtitle: None,
            badge: Some(
                if config.script.is_some() {
                    "Script"
                } else {
                    "Launcher"
                }
                .to_string(),
            ),
            icon,
            bodies,
            metadata,
        }
    }

    fn build_command_list_preview(
        &self,
        list: &CommandListState,
        item: &CommandItem,
    ) -> PreviewData {
        PreviewData {
            title: item.title.clone(),
            subtitle: None,
            badge: list.addon.map(|addon| addon.badge().to_string()),
            icon: item.icon.clone().map(PreviewIcon::File),
            bodies: super::browser_tabs::tab_target(item)
                .and_then(|tab| list.previews.get(&tab.url))
                .or_else(|| list.previews.get(&item.identity))
                .cloned()
                .unwrap_or_default(),
            metadata: item.details.clone(),
        }
    }

    fn build_main_view_preview(&self, source: usize, row: usize) -> Option<PreviewData> {
        match self.main_view.sources.get(source)? {
            MainViewSource::List(list) => list
                .item(row)
                .map(|item| self.build_command_list_preview(list, item)),
            MainViewSource::Snippets {
                config_index,
                items,
                filtered,
                previews,
                ..
            } => {
                let item = *filtered.get(row)?;
                let snippet = items.get(item)?;
                let config = self.addons.text_snippets.get(*config_index)?;
                let icon = snippet
                    .icon
                    .iter()
                    .chain(config.icon.iter())
                    .find_map(|icon| self.resolve_icon_or_glyph(icon));
                Some(self.snippet_preview(
                    snippet,
                    icon,
                    previews.get(item).and_then(Option::as_deref),
                    config.action.as_deref(),
                    config.secondary_action.as_deref(),
                ))
            }
            MainViewSource::ScriptFilter {
                config_index,
                results,
                ..
            } => {
                let result = results.as_ref()?;
                let config = self.addons.script_filters.get(*config_index)?;
                Some(self.build_script_filter_preview(
                    row,
                    result.items.get(row)?,
                    result,
                    config.action.as_deref(),
                    config.secondary_action.as_deref(),
                    false,
                ))
            }
        }
    }

    fn build_emoji_preview(&self, entry: &EmojiEntry) -> PreviewData {
        let codepoints: Vec<String> = entry
            .value
            .chars()
            .map(|c| format!("U+{:04X}", c as u32))
            .collect();
        let utf8_bytes: Vec<String> = entry
            .value
            .as_bytes()
            .iter()
            .map(|b| format!("{b:02X}"))
            .collect();

        let primary = self.emoji.action.as_deref().unwrap_or("copy");
        let secondary = self.emoji.secondary_action.as_deref().unwrap_or("insert");

        let metadata = vec![
            ("Character".to_string(), entry.value.clone()),
            ("Codepoints".to_string(), codepoints.join(" ")),
            ("UTF-8".to_string(), utf8_bytes.join(" ")),
            ("Enter".to_string(), describe_action(primary)),
            ("Ctrl+Enter".to_string(), describe_action(secondary)),
        ];

        PreviewData {
            title: entry.name.clone(),
            subtitle: None,
            badge: Some("Emoji".to_string()),
            icon: None,
            bodies: vec![PreviewBody::HeroGlyph {
                glyph: entry.value.clone(),
            }],
            metadata,
        }
    }

    fn build_clipboard_preview(&self, item: &ClipItem) -> PreviewData {
        let mut bodies = Vec::new();
        let mut metadata = vec![("Type".to_string(), item.kind.label().to_string())];
        if let Some(time) = item.copied {
            metadata.push(("Copied".to_string(), super::clipboard::full_copy_time(time)));
        }
        match self.clipboard.preview(&item.id) {
            None => bodies.push(PreviewBody::Text {
                heading: None,
                content: "Loading preview...".to_string(),
                truncated: false,
            }),
            Some(Err(error)) => bodies.push(PreviewBody::Text {
                heading: Some("Preview unavailable".to_string()),
                content: error.clone(),
                truncated: false,
            }),
            Some(Ok(preview)) => {
                match &preview.image {
                    Some(Ok(image)) => {
                        bodies.push(PreviewBody::ImageData {
                            handle: image.handle.clone(),
                        });
                        metadata.push((
                            "Dimensions".to_string(),
                            format!("{} × {} px", image.width, image.height),
                        ));
                    }
                    Some(Err(error)) => bodies.push(PreviewBody::Text {
                        heading: Some("Image".to_string()),
                        content: error.clone(),
                        truncated: false,
                    }),
                    None => {}
                }
                if !preview.files.is_empty() {
                    let content = preview
                        .files
                        .iter()
                        .map(|file| match file.missing {
                            Some(true) => format!("{} (missing)", file.name),
                            _ => file.name.clone(),
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    bodies.push(PreviewBody::Text {
                        heading: Some("Files".to_string()),
                        content,
                        truncated: false,
                    });
                } else if !preview.text.is_empty() {
                    let heading = if preview.kind == ItemKind::Html {
                        "Plain text"
                    } else {
                        "Text"
                    };
                    bodies.push(PreviewBody::Text {
                        heading: Some(heading.to_string()),
                        content: preview.text.clone(),
                        truncated: preview.text_truncated,
                    });
                }
                let formats = preview
                    .formats
                    .iter()
                    .map(|(format, size)| format!("{format} ({})", format_byte_size(*size as u64)))
                    .collect::<Vec<_>>()
                    .join(", ");
                if !formats.is_empty() {
                    metadata.push(("Formats".to_string(), formats));
                }
            }
        }
        let config = &self.addons.clipboard;
        metadata.push((
            "Enter".to_string(),
            describe_clipboard_action(config.action),
        ));
        metadata.push((
            "Ctrl+Enter".to_string(),
            describe_clipboard_action(config.secondary_action),
        ));
        let images = if self.clipboard.images_only {
            "Show all items"
        } else {
            "Show only images"
        };
        metadata.push(("Ctrl+I".to_string(), images.to_string()));
        metadata.push(("Ctrl+D".to_string(), "Delete this entry".to_string()));
        metadata.push(("Ctrl+R".to_string(), "Refresh the list".to_string()));

        PreviewData {
            title: item.title(),
            subtitle: None,
            badge: Some("Clipboard".to_string()),
            icon: None,
            bodies,
            metadata,
        }
    }

    fn build_currency_help_preview(&self) -> PreviewData {
        let trigger = self.addons.currency.trigger.as_deref().unwrap_or("$");
        let default_curr = self
            .addons
            .currency
            .default_currency
            .as_deref()
            .unwrap_or("USD");
        let targets = self
            .addons
            .currency
            .currencies
            .as_ref()
            .map(|c| c.join(", "))
            .unwrap_or_else(|| "USD, EUR, GBP".to_string());

        PreviewData {
            title: "Currency Converter".to_string(),
            subtitle: Some("Live exchange rates via Frankfurter API".to_string()),
            badge: Some("Currency".to_string()),
            icon: None,
            bodies: vec![PreviewBody::Text {
                heading: Some("Examples".to_string()),
                content: format!(
                    "{trigger}100 to EUR\n{trigger}50 GBP to USD\n{trigger}250 (converts {default_curr} to {targets})"
                ),
                truncated: false,
            }],
            metadata: vec![
                ("Trigger".to_string(), trigger.to_string()),
                ("Default Base".to_string(), default_curr.to_string()),
                ("Target Currencies".to_string(), targets),
            ],
        }
    }

    fn build_currency_single_preview(&self, currency: &CurrencyResult) -> PreviewData {
        let req = &currency.request;
        let inverse_rate = if currency.rate > 0.0 {
            1.0 / currency.rate
        } else {
            0.0
        };

        PreviewData {
            title: format!("{} → {}", req.from_currency, req.to_currency),
            subtitle: Some("Currency Conversion".to_string()),
            badge: Some("Currency".to_string()),
            icon: None,
            bodies: vec![PreviewBody::HeroValue {
                expression: Some(format!("{:.2} {}", req.amount, req.from_currency)),
                value: format!("{:.2} {}", currency.converted_amount, req.to_currency),
            }],
            metadata: vec![
                (
                    "Rate".to_string(),
                    format!(
                        "1 {} = {:.4} {}",
                        req.from_currency, currency.rate, req.to_currency
                    ),
                ),
                (
                    "Inverse Rate".to_string(),
                    format!(
                        "1 {} = {:.4} {}",
                        req.to_currency, inverse_rate, req.from_currency
                    ),
                ),
                (
                    "Enter".to_string(),
                    format!("Copy {:.2} to clipboard", currency.converted_amount),
                ),
            ],
        }
    }

    fn build_currency_multi_preview(
        &self,
        multi: &MultiCurrencyResult,
        selected: &CurrencyConversion,
    ) -> PreviewData {
        let inverse_rate = if selected.rate > 0.0 {
            1.0 / selected.rate
        } else {
            0.0
        };

        let summary_lines: Vec<String> = multi
            .conversions
            .iter()
            .map(|c| {
                format!(
                    "{:.2} {} = {:.2} {} (rate: {:.4})",
                    multi.amount, multi.from_currency, c.converted_amount, c.to_currency, c.rate
                )
            })
            .collect();

        PreviewData {
            title: format!("{} → {}", multi.from_currency, selected.to_currency),
            subtitle: Some("Multi-Currency Conversion".to_string()),
            badge: Some("Currency".to_string()),
            icon: None,
            bodies: vec![
                PreviewBody::HeroValue {
                    expression: Some(format!("{:.2} {}", multi.amount, multi.from_currency)),
                    value: format!("{:.2} {}", selected.converted_amount, selected.to_currency),
                },
                PreviewBody::Text {
                    heading: Some("All Conversions".to_string()),
                    content: summary_lines.join("\n"),
                    truncated: false,
                },
            ],
            metadata: vec![
                (
                    "Rate".to_string(),
                    format!(
                        "1 {} = {:.4} {}",
                        multi.from_currency, selected.rate, selected.to_currency
                    ),
                ),
                (
                    "Inverse Rate".to_string(),
                    format!(
                        "1 {} = {:.4} {}",
                        selected.to_currency, inverse_rate, multi.from_currency
                    ),
                ),
                (
                    "Enter".to_string(),
                    format!("Copy {:.2} to clipboard", selected.converted_amount),
                ),
            ],
        }
    }

    fn build_web_search_preview(&self, ws: &WebSearchActiveState) -> PreviewData {
        let icon = ws
            .icon
            .as_deref()
            .and_then(|name| self.resolve_named_icon(name));

        let resolved_url = if ws.query.is_empty() {
            ws.url_template.clone()
        } else {
            let encoded = crate::url_encode_query(&ws.query);
            ws.url_template.replace("{query}", &encoded)
        };

        let mut metadata = vec![("Search Engine".to_string(), ws.name.clone())];
        if !ws.query.is_empty() {
            metadata.push(("Query".to_string(), ws.query.clone()));
        }
        metadata.push(("Enter".to_string(), "Open in default browser".to_string()));

        PreviewData {
            title: ws.name.clone(),
            subtitle: Some("Web Search".to_string()),
            badge: Some("Web Search".to_string()),
            icon,
            bodies: vec![PreviewBody::Text {
                heading: Some("Target URL".to_string()),
                content: resolved_url,
                truncated: false,
            }],
            metadata,
        }
    }

    fn build_keyword_suggestion_preview(&self, suggestion: &KeywordSuggestion) -> PreviewData {
        let icon = suggestion
            .icon
            .as_deref()
            .and_then(|name| self.keyword_preview_icon(name));

        let (badge, mut metadata) = describe_addon_by_keyword(&suggestion.keyword, &self.addons);
        metadata.push((
            "Enter / Tab".to_string(),
            format!("Activate `{}` keyword", suggestion.keyword),
        ));

        PreviewData {
            title: suggestion.subtitle.clone(),
            subtitle: Some(format!("Keyword: `{}`", suggestion.keyword)),
            badge: Some(badge),
            icon,
            bodies: Vec::new(),
            metadata,
        }
    }

    fn build_fallback_preview(&self, fallback: &ResolvedFallback) -> PreviewData {
        let icon = fallback
            .icon
            .as_deref()
            .and_then(|name| self.resolve_named_icon(name));
        let query = self.search_query.trim();

        match &fallback.action {
            FallbackAction::WebSearch { url_template } => {
                let encoded = crate::url_encode_query(query);
                let url = url_template.replace("{query}", &encoded);
                PreviewData {
                    title: format!("Search {} for '{}'", fallback.name, query),
                    subtitle: Some("Fallback Web Search".to_string()),
                    badge: Some("Fallback".to_string()),
                    icon,
                    bodies: vec![PreviewBody::Text {
                        heading: Some("Target URL".to_string()),
                        content: url,
                        truncated: false,
                    }],
                    metadata: vec![
                        ("Query".to_string(), query.to_string()),
                        ("Enter".to_string(), "Open in default browser".to_string()),
                    ],
                }
            }
            FallbackAction::ScriptFilter {
                keyword,
                command,
                args,
                action,
                ..
            } => {
                let mut full_cmd = vec![command.clone()];
                full_cmd.extend(args.iter().cloned());
                full_cmd.push(query.to_string());
                PreviewData {
                    title: format!("Search {} for '{}'", fallback.name, query),
                    subtitle: Some("Fallback Script Filter".to_string()),
                    badge: Some("Fallback".to_string()),
                    icon,
                    bodies: vec![PreviewBody::Text {
                        heading: Some("Command".to_string()),
                        content: full_cmd.join(" "),
                        truncated: false,
                    }],
                    metadata: vec![
                        ("Keyword".to_string(), keyword.clone()),
                        (
                            "Enter".to_string(),
                            describe_action(action.as_deref().unwrap_or("copy")),
                        ),
                    ],
                }
            }
            FallbackAction::Terminal { prefix, hold } => PreviewData {
                title: fallback.label(query),
                subtitle: Some("Fallback Terminal".to_string()),
                badge: Some("Fallback".to_string()),
                icon,
                bodies: vec![PreviewBody::Text {
                    heading: Some("Terminal".to_string()),
                    content: prefix.join(" "),
                    truncated: false,
                }],
                metadata: vec![
                    ("Query".to_string(), query.to_string()),
                    (
                        "Enter".to_string(),
                        if *hold {
                            "Run in a terminal kept open".to_string()
                        } else {
                            "Run in a terminal".to_string()
                        },
                    ),
                ],
            },
        }
    }

    fn build_help_preview(&self, entry: &HelpEntry) -> PreviewData {
        let icon = entry
            .icon
            .as_deref()
            .and_then(|name| self.keyword_preview_icon(name));

        let (badge, mut metadata) = describe_addon_by_keyword(&entry.trigger, &self.addons);
        metadata.insert(0, ("Usage".to_string(), entry.usage.clone()));
        if let Some(completion) = &entry.completion {
            metadata.push((
                "Enter / Tab".to_string(),
                format!("Insert `{completion}` into search bar"),
            ));
        }

        PreviewData {
            title: entry.title.clone(),
            subtitle: Some(format!("Trigger: `{}`", entry.trigger)),
            badge: Some(badge),
            icon,
            bodies: Vec::new(),
            metadata,
        }
    }
}

fn vault_key_rows() -> Vec<(String, String)> {
    [
        ("Enter", "Copy the password"),
        ("Ctrl+Enter", "Type the password"),
        ("Ctrl+U", "Copy the username"),
        ("Ctrl+O", "Copy the OTP code"),
    ]
    .into_iter()
    .map(|(key, action)| (key.to_string(), action.to_string()))
    .collect()
}

/// Inspect `AddonsConfig` to provide rich metadata for a keyword suggestion or `Ctrl+/` help entry.
fn describe_addon_by_keyword(
    keyword: &str,
    addons: &AddonsConfig,
) -> (String, Vec<(String, String)>) {
    if keyword == "~/" && addons.file_browser.enabled {
        return (
            "File Browser".to_string(),
            vec![
                (
                    "Show Hidden".to_string(),
                    addons.file_browser.show_hidden.unwrap_or(false).to_string(),
                ),
                (
                    "Shortcut".to_string(),
                    "Ctrl+H toggles dotfiles".to_string(),
                ),
                (
                    "Sort".to_string(),
                    format!(
                        "{}{} (Ctrl+S cycles)",
                        addons.file_browser.sort.label(),
                        if addons.file_browser.sort_reverse {
                            ", reversed"
                        } else {
                            ""
                        }
                    ),
                ),
                (
                    "Show".to_string(),
                    format!("{} (Ctrl+. cycles)", addons.file_browser.show.label()),
                ),
                (
                    "Enter".to_string(),
                    describe_file_action(addons.file_browser.action()),
                ),
                (
                    "Ctrl+Enter".to_string(),
                    describe_file_action(addons.file_browser.secondary_action()),
                ),
            ],
        );
    }

    if let Some(sf) = addons.script_filters.iter().find(|s| s.keyword == keyword) {
        let mut meta = vec![("Command".to_string(), sf.command.clone())];
        if !sf.args.is_empty() {
            meta.push(("Arguments".to_string(), sf.args.join(" ")));
        }
        if let Some(min) = sf.min_query_length {
            meta.push(("Min Query Length".to_string(), min.to_string()));
        }
        meta.push((
            "Enter".to_string(),
            describe_action(sf.action.as_deref().unwrap_or("copy")),
        ));
        return ("Script Filter".to_string(), meta);
    }

    if let Some(ts) = addons.text_snippets.iter().find(|s| s.keyword == keyword) {
        let source_desc = if let Some(snippets) = &ts.snippets {
            format!("{} inline snippets", snippets.len())
        } else if let Some(file) = &ts.file {
            format!("File: {file}")
        } else if let Some(dir) = &ts.directory {
            format!("Directory: {dir}")
        } else if let Some(cmd) = &ts.command {
            format!("Command: {cmd}")
        } else {
            "Configured source".to_string()
        };
        let meta = vec![
            ("Source".to_string(), source_desc),
            ("Placeholders".to_string(), ts.placeholders.to_string()),
            (
                "Enter".to_string(),
                describe_action(ts.action.as_deref().unwrap_or("copy")),
            ),
        ];
        return ("Text Snippets".to_string(), meta);
    }

    let emoji_trigger = addons.emoji.trigger.as_deref().unwrap_or("emoji");
    if addons.emoji.enabled && keyword == emoji_trigger {
        return (
            "Emoji Picker".to_string(),
            vec![
                (
                    "Enter".to_string(),
                    describe_action(addons.emoji.action.as_deref().unwrap_or("copy")),
                ),
                (
                    "Ctrl+Enter".to_string(),
                    describe_action(addons.emoji.secondary_action.as_deref().unwrap_or("insert")),
                ),
            ],
        );
    }

    if let Some(ws) = addons.web_searches.iter().find(|w| w.keyword == keyword) {
        return (
            "Web Search".to_string(),
            vec![("URL Template".to_string(), ws.url.clone())],
        );
    }

    if addons.system.enabled && keyword == addons.system.trigger() {
        return (
            "System Commands".to_string(),
            vec![(
                "Enter".to_string(),
                "Run; every entry but Lock asks for a second Enter".to_string(),
            )],
        );
    }

    if addons.windows.enabled && keyword == addons.windows.trigger() {
        return (
            "Window Switcher".to_string(),
            vec![
                ("Enter".to_string(), "Focus the window".to_string()),
                (
                    "Ctrl+Enter".to_string(),
                    "Move it to the current workspace and focus it".to_string(),
                ),
                ("Requires".to_string(), "Hyprland, niri or sway".to_string()),
            ],
        );
    }

    if addons.browser_tabs.enabled && keyword == addons.browser_tabs.trigger() {
        return (
            "Browser Tabs".to_string(),
            vec![
                ("Enter".to_string(), "Switch to the tab".to_string()),
                ("Ctrl+Enter".to_string(), "Close the tab".to_string()),
                (
                    "Requires".to_string(),
                    "The Vicinae browser extension and raffi --install-browser-host".to_string(),
                ),
            ],
        );
    }

    if addons.media.enabled && keyword == addons.media.trigger() {
        return (
            "Media Controls".to_string(),
            vec![
                (
                    "Enter".to_string(),
                    "Play, pause, skip or stop the player".to_string(),
                ),
                ("Requires".to_string(), "playerctl".to_string()),
            ],
        );
    }

    if addons.audio.enabled && keyword == addons.audio.trigger() {
        return (
            "Audio Devices".to_string(),
            vec![
                (
                    "Enter".to_string(),
                    "Make the device the default".to_string(),
                ),
                (
                    "Ctrl+Enter".to_string(),
                    "Mute or unmute the device".to_string(),
                ),
                ("Requires".to_string(), "pactl".to_string()),
            ],
        );
    }

    if addons.kill.enabled && keyword == addons.kill.trigger() {
        return (
            "Kill Process".to_string(),
            vec![
                (
                    "Enter".to_string(),
                    "Press twice to send SIGTERM".to_string(),
                ),
                (
                    "Ctrl+Enter".to_string(),
                    "Press twice to send SIGKILL".to_string(),
                ),
                ("Requires".to_string(), "kill".to_string()),
            ],
        );
    }

    if addons.bluetooth.enabled && keyword == addons.bluetooth.trigger() {
        return (
            "Bluetooth".to_string(),
            vec![
                (
                    "Enter".to_string(),
                    "Connect a device, or press twice to disconnect it".to_string(),
                ),
                (
                    "First row".to_string(),
                    "Turn Bluetooth on, or press twice to turn it off".to_string(),
                ),
                ("Requires".to_string(), "bluetoothctl".to_string()),
            ],
        );
    }

    if addons.pass.enabled && keyword == addons.pass.trigger() {
        let mut rows = vault_key_rows();
        rows.push(("Requires".to_string(), "pass, gpg".to_string()));
        return ("Password Store".to_string(), rows);
    }

    if addons.bitwarden.enabled && keyword == addons.bitwarden.trigger() {
        let mut rows = vault_key_rows();
        rows.push(("Requires".to_string(), "rbw".to_string()));
        return ("Bitwarden".to_string(), rows);
    }

    if addons.slack.enabled && keyword == addons.slack.trigger() {
        let trigger = addons.slack.trigger();
        return (
            "Slack".to_string(),
            vec![
                (
                    "Usage".to_string(),
                    format!("{trigger} <name> opens a channel, DM or person"),
                ),
                (
                    "Search".to_string(),
                    format!("{trigger} search <words>"),
                ),
                ("People".to_string(), format!("{trigger} people <name>")),
                ("Unread".to_string(), format!("{trigger} unread")),
                (
                    "Status".to_string(),
                    format!("{trigger} status <text> [:emoji:] [30m|2h|today]"),
                ),
                (
                    "Ctrl+Enter".to_string(),
                    "Open in the other app (browser or desktop), or mark an unread conversation read".to_string(),
                ),
            ],
        );
    }

    if addons.devtools.enabled && keyword == addons.devtools.trigger() {
        return (
            "Developer Tools".to_string(),
            vec![
                (
                    "Transforms".to_string(),
                    "Base64, URL encoding, hashes, epoch, JSON, case".to_string(),
                ),
                ("Enter".to_string(), "Copy the result".to_string()),
            ],
        );
    }

    if keyword == "math" && addons.calculator.enabled {
        return (
            "Calculator".to_string(),
            vec![(
                "Operators".to_string(),
                "+, -, *, /, %, ^, parentheses, sin, cos, sqrt".to_string(),
            )],
        );
    }

    if keyword == "units" && addons.units.enabled {
        return (
            "Unit Converter".to_string(),
            vec![
                (
                    "Units".to_string(),
                    "length, mass, volume, temperature, data, time, speed, area".to_string(),
                ),
                (
                    "Ctrl+Enter".to_string(),
                    "Copy the result with its unit".to_string(),
                ),
            ],
        );
    }

    if keyword == "color" && addons.color.enabled {
        return (
            "Color Converter".to_string(),
            vec![
                (
                    "Input".to_string(),
                    "#rgb, #rrggbb, rgb(), rgba(), hsl(), hsla()".to_string(),
                ),
                (
                    "Output".to_string(),
                    "Hex, RGB, HSL and the nearest CSS color name".to_string(),
                ),
            ],
        );
    }

    if keyword == "time" && addons.world_clock.enabled {
        return (
            "World Clock".to_string(),
            vec![
                (
                    "Input".to_string(),
                    "time <place>, time in <place>, <time> <place> to <place>".to_string(),
                ),
                (
                    "Output".to_string(),
                    "The time in each matching time zone".to_string(),
                ),
            ],
        );
    }

    let curr_trigger = addons.currency.trigger.as_deref().unwrap_or("$");
    if addons.currency.enabled && keyword == curr_trigger {
        return (
            "Currency".to_string(),
            vec![(
                "Default Currency".to_string(),
                addons
                    .currency
                    .default_currency
                    .clone()
                    .unwrap_or_else(|| "USD".to_string()),
            )],
        );
    }

    ("Addon".to_string(), Vec::new())
}

fn devtools_preview(row: &DevRow) -> PreviewData {
    let (bodies, enter) = if row.copyable {
        (
            vec![PreviewBody::Text {
                heading: None,
                content: row.value.clone(),
                truncated: false,
            }],
            "Copy to clipboard",
        )
    } else {
        (Vec::new(), "Nothing to copy")
    };
    PreviewData {
        title: row.title.to_string(),
        subtitle: (!row.copyable).then(|| row.value.clone()),
        badge: Some("Developer Tools".to_string()),
        icon: None,
        bodies,
        metadata: vec![("Enter".to_string(), enter.to_string())],
    }
}
