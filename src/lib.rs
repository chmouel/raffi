use std::{
    collections::{HashMap, HashSet},
    fmt::Write as _,
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result};
use gumdrop::Options;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_yaml::Value;

pub mod browser_host;
pub mod debug;
#[cfg(feature = "wayland")]
pub mod secret;
pub mod terminal;
pub mod ui;

/// Represents the configuration for each Raffi entry.
#[derive(Deserialize, JsonSchema, Debug, PartialEq, Clone, Default)]
#[schemars(deny_unknown_fields)]
pub struct RaffiConfig {
    pub binary: Option<String>,
    pub args: Option<Vec<String>>,
    pub icon: Option<String>,
    pub description: Option<String>,
    pub ifenveq: Option<Vec<String>>,
    pub ifenvset: Option<String>,
    pub ifenvnotset: Option<String>,
    pub ifexist: Option<String>,
    pub disabled: Option<bool>,
    pub script: Option<String>,
    /// Show the right-hand preview pane when this launcher entry is selected (default: false).
    pub show_preview: Option<bool>,
    /// Route this launcher to an internal command: picking it types this text
    /// for you, such as `mp linus/linux` (an addon keyword
    /// followed by its query). Cannot be combined with `binary`, `script` or
    /// `args`.
    pub alias: Option<String>,
    /// Keyword that stands for `alias` while typing: with keyword `linoux`,
    /// typing `linoux torvalds` searches `mp linus/linux torvalds`
    /// while the input keeps showing `linoux torvalds`. Requires `alias`; no
    /// whitespace.
    pub keyword: Option<String>,
    /// Close Raffi after launching this entry (default: `general.close_on_action`, else true).
    pub close_on_action: Option<bool>,
}

/// A launcher `keyword` and the `alias` text it expands to.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LauncherKeyword {
    pub keyword: String,
    pub alias: String,
    pub description: String,
    pub icon: Option<String>,
}

/// Keywords of the launchers in `entries`; the first launcher wins a keyword
/// used twice.
fn launcher_keywords(entries: &[RaffiConfig]) -> Vec<LauncherKeyword> {
    let mut keywords: Vec<LauncherKeyword> = Vec::new();
    for entry in entries {
        let (Some(keyword), Some(alias)) = (&entry.keyword, &entry.alias) else {
            continue;
        };
        if keywords.iter().any(|known| &known.keyword == keyword) {
            eprintln!(
                "raffi: config warning: launcher keyword '{keyword}' is used more than once, keeping the first"
            );
            continue;
        }
        keywords.push(LauncherKeyword {
            keyword: keyword.clone(),
            alias: alias.clone(),
            description: entry.description.clone().unwrap_or_default(),
            icon: entry.icon.clone(),
        });
    }
    keywords
}

/// Configuration for the currency addon
#[derive(Deserialize, JsonSchema, Debug, Clone)]
#[schemars(deny_unknown_fields)]
pub struct CurrencyAddonConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub currencies: Option<Vec<String>>,
    #[serde(default)]
    pub default_currency: Option<String>,
    #[serde(default)]
    pub trigger: Option<String>,
    /// Close Raffi after copying a conversion result (default: `general.close_on_action`, else true).
    #[serde(default)]
    pub close_on_action: Option<bool>,
}

impl Default for CurrencyAddonConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            currencies: None,
            default_currency: None,
            trigger: None,
            close_on_action: None,
        }
    }
}

/// Configuration for the calculator addon
#[derive(Deserialize, JsonSchema, Debug, Clone)]
#[schemars(deny_unknown_fields)]
pub struct CalculatorAddonConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Close Raffi after copying a calculation result (default: `general.close_on_action`, else true).
    #[serde(default)]
    pub close_on_action: Option<bool>,
}

impl Default for CalculatorAddonConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            close_on_action: None,
        }
    }
}

/// Configuration for the inline unit converter addon
#[derive(Deserialize, JsonSchema, Debug, Clone)]
#[schemars(deny_unknown_fields)]
pub struct UnitsAddonConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Close Raffi after copying a converted value (default: `general.close_on_action`, else true).
    #[serde(default)]
    pub close_on_action: Option<bool>,
}

impl Default for UnitsAddonConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            close_on_action: None,
        }
    }
}

/// Configuration for the inline color converter addon
#[derive(Deserialize, JsonSchema, Debug, Clone)]
#[schemars(deny_unknown_fields)]
pub struct ColorAddonConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Close Raffi after copying a color value (default: `general.close_on_action`, else true).
    #[serde(default)]
    pub close_on_action: Option<bool>,
}

impl Default for ColorAddonConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            close_on_action: None,
        }
    }
}

/// Configuration for the developer tools addon: the `dev` transform list
/// and the inline `uuid` and `pw` generators
#[derive(Deserialize, JsonSchema, Debug, Clone)]
#[schemars(deny_unknown_fields)]
pub struct DevtoolsAddonConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Keyword that opens the transform list (default: `dev`).
    #[serde(default)]
    pub trigger: Option<String>,
    /// Length of passwords generated by `pw` without a length, from 8 to 128
    /// (default: 20).
    #[serde(default)]
    #[schemars(range(min = 8, max = 128))]
    pub password_length: Option<usize>,
    /// Close Raffi after copying a generated or transformed value (default: `general.close_on_action`, else true).
    #[serde(default)]
    pub close_on_action: Option<bool>,
}

impl DevtoolsAddonConfig {
    pub fn trigger(&self) -> &str {
        self.trigger.as_deref().unwrap_or("dev")
    }

    pub fn password_length(&self) -> usize {
        self.password_length.unwrap_or(20).clamp(8, 128)
    }
}

impl Default for DevtoolsAddonConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            trigger: None,
            password_length: None,
            close_on_action: None,
        }
    }
}

/// Configuration for the inline world clock addon
#[derive(Deserialize, JsonSchema, Debug, Clone)]
#[schemars(deny_unknown_fields)]
pub struct WorldClockAddonConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Close Raffi after copying a time value (default: `general.close_on_action`, else true).
    #[serde(default)]
    pub close_on_action: Option<bool>,
}

impl Default for WorldClockAddonConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            close_on_action: None,
        }
    }
}

/// Configuration for the file browser addon
#[derive(Deserialize, JsonSchema, Debug, Clone)]
#[schemars(deny_unknown_fields)]
pub struct FileBrowserAddonConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub show_hidden: Option<bool>,
    /// Edit paths the way Emacs fido-mode does: Backspace on a trailing `/`
    /// goes up to the parent directory, and typing `~/` or `/` right after a
    /// `/` starts over from home or the root. Off, the input is plain text.
    #[serde(default = "default_true")]
    pub fido_style: bool,
    /// Action on Enter for files: `open` (xdg-open), `copy`, `insert`, or a
    /// shell command where `{value}` is the full path. Defaults to `open`.
    #[serde(default)]
    pub action: Option<String>,
    /// Action on Ctrl+Enter for files and directories, with the same values as
    /// `action`. Defaults to `copy`.
    #[serde(default)]
    pub secondary_action: Option<String>,
    /// File manager opened with Ctrl+B on the browsed directory. `{value}` is
    /// replaced by the directory, otherwise it is appended as the last
    /// argument. Nautilus, Dolphin, Nemo and Caja also get the selected entry
    /// highlighted. Autodetected when unset.
    #[serde(default)]
    pub file_manager: Option<String>,
    /// Initial order of the listing; Ctrl+S cycles through the modes.
    /// Directories are always listed first.
    #[serde(default)]
    pub sort: FileBrowserSort,
    /// Reverse the sort order: Z to A, or oldest first for date sorts.
    /// Directories stay first and Ctrl+S keeps this setting.
    #[serde(default)]
    pub sort_reverse: bool,
    /// Initial filter of the listing: `all`, `files` or `directories`;
    /// Ctrl+Tab cycles through them.
    #[serde(default)]
    pub show: FileBrowserShow,
    /// Command that previews the selected entry in the preview pane.
    /// Unset uses the previewer bundled with raffi when python3 is
    /// installed; `none` turns it off. A command runs through `sh -c` with
    /// the path appended as the last argument (also in `$RAFFI_VALUE`) and
    /// prints ANSI text or a Script Filter preview object; no output or a
    /// non-zero exit keeps the built-in preview. Killed after 3 seconds.
    #[serde(default)]
    pub preview_command: Option<String>,
    /// Close Raffi after executing a file browser action (default: `general.close_on_action`, else true).
    #[serde(default)]
    pub close_on_action: Option<bool>,
}

/// Order of file browser entries
#[derive(Deserialize, JsonSchema, Debug, Clone, Copy, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum FileBrowserSort {
    /// Alphabetical, case-insensitive
    #[default]
    Name,
    /// Last modification time, newest first (`date` is accepted as an alias)
    #[serde(alias = "date")]
    Modified,
    /// Creation time, newest first, where the filesystem records it
    Created,
}

impl FileBrowserSort {
    /// The mode after this one, wrapping around.
    pub fn next(self) -> Self {
        match self {
            Self::Name => Self::Modified,
            Self::Modified => Self::Created,
            Self::Created => Self::Name,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Modified => "modified",
            Self::Created => "created",
        }
    }
}

/// Which entries the file browser lists
#[derive(Deserialize, JsonSchema, Debug, Clone, Copy, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum FileBrowserShow {
    /// Directories and files
    #[default]
    All,
    /// Files only
    Files,
    /// Directories only (`dirs` and `directory` are accepted as aliases)
    #[serde(alias = "dirs", alias = "directory")]
    #[schemars(transform = accept_directories_aliases)]
    Directories,
}

fn accept_directories_aliases(schema: &mut schemars::Schema) {
    schema.remove("const");
    schema.insert(
        "enum".into(),
        serde_json::json!(["directories", "dirs", "directory"]),
    );
}

impl FileBrowserShow {
    /// The mode after this one, or before it when `backwards`, wrapping
    /// around.
    pub fn cycle(self, backwards: bool) -> Self {
        match (self, backwards) {
            (Self::All, false) | (Self::Directories, true) => Self::Files,
            (Self::Files, false) | (Self::All, true) => Self::Directories,
            (Self::Directories, false) | (Self::Files, true) => Self::All,
        }
    }

    /// Name shown after Ctrl+Tab in the footer.
    pub fn title(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Files => "Files",
            Self::Directories => "Directories",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Files => "files",
            Self::Directories => "directories",
        }
    }

    pub fn includes(self, is_dir: bool) -> bool {
        match self {
            Self::All => true,
            Self::Files => !is_dir,
            Self::Directories => is_dir,
        }
    }
}

impl FileBrowserAddonConfig {
    pub fn action(&self) -> &str {
        self.action.as_deref().unwrap_or("open")
    }

    pub fn secondary_action(&self) -> &str {
        self.secondary_action.as_deref().unwrap_or("copy")
    }
}

impl Default for FileBrowserAddonConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            show_hidden: None,
            fido_style: true,
            action: None,
            secondary_action: None,
            file_manager: None,
            sort: FileBrowserSort::default(),
            sort_reverse: false,
            show: FileBrowserShow::default(),
            preview_command: None,
            close_on_action: None,
        }
    }
}

/// Default data file names downloaded from the rofimoji project when
/// the user does not provide an explicit `data_files` list.
pub const DEFAULT_EMOJI_FILES: &[&str] = &[
    "emojis_smileys_emotion",
    "emojis_people_body",
    "emojis_animals_nature",
    "emojis_food_drink",
    "emojis_travel_places",
    "emojis_activities",
    "emojis_objects",
    "emojis_symbols",
    "emojis_flags",
    "emojis_component",
];

/// Configuration for the emoji picker addon
#[derive(Deserialize, JsonSchema, Debug, Clone)]
#[schemars(deny_unknown_fields)]
pub struct EmojiAddonConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub trigger: Option<String>,
    #[serde(default)]
    pub action: Option<String>,
    #[serde(default)]
    pub secondary_action: Option<String>,
    #[serde(default)]
    pub data_files: Option<Vec<String>>,
    /// Close Raffi after executing an emoji action (default: `general.close_on_action`, else true).
    #[serde(default)]
    pub close_on_action: Option<bool>,
}

impl Default for EmojiAddonConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            trigger: None,
            action: None,
            secondary_action: None,
            data_files: None,
            close_on_action: None,
        }
    }
}

/// Configuration for a script filter addon
#[derive(Deserialize, JsonSchema, Debug, Clone, Default)]
#[schemars(deny_unknown_fields)]
pub struct ScriptFilterConfig {
    pub name: String,
    pub command: String,
    pub keyword: String,
    pub icon: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    pub action: Option<String>,
    pub secondary_action: Option<String>,
    #[serde(default)]
    pub env: std::collections::HashMap<String, String>,
    pub min_query_length: Option<usize>,
    /// Re-run the command with `RAFFI_PREVIEW=1` for the selected item and show
    /// the preview object it prints (default: true).
    pub dynamic_preview: Option<bool>,
    /// Close Raffi after executing an action on a result (default: `general.close_on_action`, else true).
    #[serde(default)]
    pub close_on_action: Option<bool>,
}

/// Configuration for a web search addon
#[derive(Deserialize, JsonSchema, Debug, Clone, Default)]
#[schemars(deny_unknown_fields)]
pub struct WebSearchConfig {
    pub name: String,
    pub keyword: String,
    pub url: String,
    pub icon: Option<String>,
    /// Close Raffi after opening a web search URL (default: `general.close_on_action`, else true).
    #[serde(default)]
    pub close_on_action: Option<bool>,
}

/// A single text snippet entry
#[derive(Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[schemars(deny_unknown_fields)]
pub struct TextSnippet {
    pub name: String,
    pub value: String,
    /// Emoji, icon name, or image path overriding the source icon.
    #[serde(default)]
    pub icon: Option<String>,
    /// Optional short keyword; an exact match ranks the snippet first.
    #[serde(default)]
    pub keyword: Option<String>,
}

/// Configuration for a text snippet source
#[derive(Deserialize, JsonSchema, Debug, Clone, Default)]
#[schemars(deny_unknown_fields)]
pub struct TextSnippetSourceConfig {
    pub name: String,
    pub keyword: String,
    #[serde(default)]
    pub icon: Option<String>,
    #[serde(default)]
    pub snippets: Option<Vec<TextSnippet>>,
    #[serde(default)]
    pub file: Option<String>,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub directory: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub action: Option<String>,
    #[serde(default)]
    pub secondary_action: Option<String>,
    /// Expand dynamic placeholders such as {clipboard} and {date} in snippet values.
    #[serde(default = "default_true")]
    pub placeholders: bool,
    /// Best-effort restoration of plain text after the `hypr_paste` action.
    #[serde(default)]
    pub restore_clipboard: bool,
    /// Close Raffi after executing a snippet action (default: `general.close_on_action`, else true).
    #[serde(default)]
    pub close_on_action: Option<bool>,
}

/// Clipboard manager that stores the history browsed by the clipboard addon
#[derive(Deserialize, JsonSchema, Debug, Clone, Copy, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ClipboardBackend {
    /// CopyQ, through its command line scripting API
    #[default]
    Copyq,
}

/// What the clipboard addon does with the selected history item
#[derive(Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ClipboardAction {
    /// Put the item back on the clipboard with all of its formats
    Copy,
    /// Copy the item, then paste it into the window that had focus before
    /// Raffi opened (Hyprland only)
    HyprPaste,
}

/// Configuration for the clipboard history addon
#[derive(Deserialize, JsonSchema, Debug, Clone)]
#[schemars(deny_unknown_fields)]
pub struct ClipboardAddonConfig {
    /// Unset turns it on when a CopyQ process is running at startup;
    /// `true` or `false` overrides the detection.
    #[serde(default)]
    pub enabled: Option<bool>,
    /// Keyword that opens the history (default: `clip`).
    #[serde(default)]
    pub trigger: Option<String>,
    /// Hide text entries shorter than this many characters, ignoring
    /// surrounding whitespace (default: 3). Images and files are always shown.
    #[serde(default)]
    pub min_chars: Option<usize>,
    #[serde(default)]
    pub backend: ClipboardBackend,
    /// Action on Enter (default: `copy`).
    #[serde(default = "default_clipboard_action")]
    pub action: ClipboardAction,
    /// Action on Ctrl+Enter (default: `hypr_paste`).
    #[serde(default = "default_clipboard_secondary_action")]
    pub secondary_action: ClipboardAction,
    /// Close Raffi after copying a clipboard entry (default: `general.close_on_action`, else true).
    #[serde(default)]
    pub close_on_action: Option<bool>,
}

fn default_clipboard_action() -> ClipboardAction {
    ClipboardAction::Copy
}

fn default_clipboard_secondary_action() -> ClipboardAction {
    ClipboardAction::HyprPaste
}

impl ClipboardAddonConfig {
    pub fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(false)
    }

    pub fn trigger(&self) -> &str {
        self.trigger.as_deref().unwrap_or("clip")
    }

    pub fn min_chars(&self) -> usize {
        self.min_chars.unwrap_or(3)
    }

    /// Whether either action needs the Hyprland paste target.
    pub fn uses_hypr_paste(&self) -> bool {
        self.is_enabled()
            && (self.action == ClipboardAction::HyprPaste
                || self.secondary_action == ClipboardAction::HyprPaste)
    }
}

impl Default for ClipboardAddonConfig {
    fn default() -> Self {
        Self {
            enabled: None,
            trigger: None,
            min_chars: None,
            backend: ClipboardBackend::default(),
            action: default_clipboard_action(),
            secondary_action: default_clipboard_secondary_action(),
            close_on_action: None,
        }
    }
}

/// A built-in entry of the system commands addon
#[derive(Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SystemCommand {
    Lock,
    Suspend,
    Hibernate,
    Logout,
    Reboot,
    Shutdown,
}

/// Commands replacing the built-in ones of the system commands addon, as
/// argument lists run without a shell. An empty list hides the entry.
#[derive(Deserialize, JsonSchema, Debug, Clone, Default)]
#[schemars(deny_unknown_fields)]
pub struct SystemCommandsConfig {
    /// Default: `loginctl lock-session`.
    #[serde(default)]
    pub lock: Option<Vec<String>>,
    /// Default: `systemctl suspend`.
    #[serde(default)]
    pub suspend: Option<Vec<String>>,
    /// Default: `systemctl hibernate`.
    #[serde(default)]
    pub hibernate: Option<Vec<String>>,
    /// Default: Hyprland's exit dispatcher on Hyprland (through `hyprctl eval`
    /// with a Lua config, `hyprctl dispatch exit` otherwise),
    /// `niri msg action quit --skip-confirmation` on niri, `swaymsg exit` on
    /// sway, otherwise
    /// `loginctl terminate-session $XDG_SESSION_ID`.
    #[serde(default)]
    pub logout: Option<Vec<String>>,
    /// Default: `systemctl reboot`.
    #[serde(default)]
    pub reboot: Option<Vec<String>>,
    /// Default: `systemctl poweroff`.
    #[serde(default)]
    pub shutdown: Option<Vec<String>>,
}

impl SystemCommandsConfig {
    pub fn get(&self, command: SystemCommand) -> Option<&Vec<String>> {
        match command {
            SystemCommand::Lock => self.lock.as_ref(),
            SystemCommand::Suspend => self.suspend.as_ref(),
            SystemCommand::Hibernate => self.hibernate.as_ref(),
            SystemCommand::Logout => self.logout.as_ref(),
            SystemCommand::Reboot => self.reboot.as_ref(),
            SystemCommand::Shutdown => self.shutdown.as_ref(),
        }
    }
}

/// Configuration for the system commands addon
#[derive(Deserialize, JsonSchema, Debug, Clone)]
#[schemars(deny_unknown_fields)]
pub struct SystemAddonConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Keyword that opens the list (default: `sys`).
    #[serde(default)]
    pub trigger: Option<String>,
    #[serde(default)]
    pub commands: SystemCommandsConfig,
    /// Entries left out of the list.
    #[serde(default)]
    pub hide: Vec<SystemCommand>,
    /// Close Raffi after running a system command (default: `general.close_on_action`, else true).
    #[serde(default)]
    pub close_on_action: Option<bool>,
}

impl SystemAddonConfig {
    pub fn trigger(&self) -> &str {
        self.trigger.as_deref().unwrap_or("sys")
    }
}

impl Default for SystemAddonConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            trigger: None,
            commands: SystemCommandsConfig::default(),
            hide: Vec::new(),
            close_on_action: None,
        }
    }
}

/// Configuration for the window switcher addon for Hyprland, niri and sway
#[derive(Deserialize, JsonSchema, Debug, Clone)]
#[schemars(deny_unknown_fields)]
pub struct WindowsAddonConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Keyword that opens the window list (default: `win`).
    #[serde(default)]
    pub trigger: Option<String>,
    /// Close Raffi after focusing or moving a window (default: `general.close_on_action`, else true).
    #[serde(default)]
    pub close_on_action: Option<bool>,
}

impl WindowsAddonConfig {
    pub fn trigger(&self) -> &str {
        self.trigger.as_deref().unwrap_or("win")
    }
}

impl Default for WindowsAddonConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            trigger: None,
            close_on_action: None,
        }
    }
}

/// Configuration for the browser tabs addon, fed by the Vicinae browser
/// extension
#[derive(Deserialize, JsonSchema, Debug, Clone)]
#[schemars(deny_unknown_fields)]
pub struct BrowserTabsAddonConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Keyword that opens the tab list (default: `tabs`).
    #[serde(default)]
    pub trigger: Option<String>,
    /// Where to download site icons the browsers have not stored themselves
    /// (default: `twenty`). Downloading sends the domains of open tabs to
    /// that service; `none` keeps favicons to the browsers' own and stops
    /// fetching the selected page for its preview.
    #[serde(default)]
    pub favicon_service: FaviconService,
    /// Close Raffi after switching to a tab (default: `general.close_on_action`, else true).
    #[serde(default)]
    pub close_on_action: Option<bool>,
}

/// Service asked for the favicons of sites the browsers have none for
#[derive(Deserialize, JsonSchema, Debug, Clone, Copy, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum FaviconService {
    /// twenty-icons.com, the Vicinae default
    #[default]
    Twenty,
    /// Google's favicon service
    Google,
    /// Never download; only use what the browsers stored
    None,
}

impl BrowserTabsAddonConfig {
    pub fn trigger(&self) -> &str {
        self.trigger.as_deref().unwrap_or("tabs")
    }
}

impl Default for BrowserTabsAddonConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            trigger: None,
            favicon_service: FaviconService::default(),
            close_on_action: None,
        }
    }
}

/// A browser the bookmarks and browser history addons read from or open
/// links with
#[derive(Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "kebab-case")]
pub enum BookmarkBrowser {
    Firefox,
    Librewolf,
    Zen,
    Floorp,
    Waterfox,
    Chrome,
    ChromeBeta,
    ChromeDev,
    Chromium,
    Brave,
    Edge,
    Vivaldi,
    Opera,
    Thorium,
}

/// What opens a bookmark: a browser name, or a command whose `{url}`
/// arguments are replaced by the link
#[derive(Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[serde(untagged)]
pub enum BookmarkOpener {
    Browser(BookmarkBrowser),
    Command(Vec<String>),
}

/// One place bookmarks are read from
#[derive(Deserialize, JsonSchema, Debug, Clone, Default, PartialEq, Eq)]
#[schemars(deny_unknown_fields)]
pub struct BookmarkSourceConfig {
    /// Browser whose profiles hold the bookmarks. Optional with `path`,
    /// where it only picks the browser that opens the links.
    #[serde(default)]
    pub browser: Option<BookmarkBrowser>,
    /// Profile to read, by name or directory (default: every profile).
    #[serde(default)]
    pub profile: Option<String>,
    /// A bookmarks file read directly: Chromium's `Bookmarks`, Firefox's
    /// `places.sqlite`, or the profile directory holding one.
    #[serde(default)]
    pub path: Option<String>,
    /// Only list this folder, such as `Bookmarks Toolbar/Work`.
    #[serde(default)]
    pub folder: Option<String>,
    /// Label of the source in the list (default: the browser and profile).
    #[serde(default)]
    pub name: Option<String>,
    /// Opens this source's bookmarks (default: `addons.bookmarks.open_with`,
    /// else the browser and profile they come from).
    #[serde(default)]
    pub open_with: Option<BookmarkOpener>,
}

/// Configuration for the browser bookmarks addon (native UI only)
#[derive(Deserialize, JsonSchema, Debug, Clone)]
#[schemars(deny_unknown_fields)]
pub struct BookmarksAddonConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Keyword that opens the bookmarks (default: `bm`).
    #[serde(default)]
    pub trigger: Option<String>,
    /// Opens bookmarks of every source without its own `open_with`
    /// (default: the browser and profile they come from).
    #[serde(default)]
    pub open_with: Option<BookmarkOpener>,
    /// Where bookmarks come from (default: every profile of every supported
    /// browser found).
    #[serde(default)]
    pub sources: Option<Vec<BookmarkSourceConfig>>,
    /// Close Raffi after opening a bookmark (default: `general.close_on_action`, else true).
    #[serde(default)]
    pub close_on_action: Option<bool>,
}

impl BookmarksAddonConfig {
    pub fn trigger(&self) -> &str {
        self.trigger.as_deref().unwrap_or("bm")
    }
}

impl Default for BookmarksAddonConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            trigger: None,
            open_with: None,
            sources: None,
            close_on_action: None,
        }
    }
}

/// One place browser history is read from
#[derive(Deserialize, JsonSchema, Debug, Clone, Default, PartialEq, Eq)]
#[schemars(deny_unknown_fields)]
pub struct BrowserHistorySourceConfig {
    /// Browser whose profiles hold the history. Optional with `path`,
    /// where it only picks the browser that opens the links.
    #[serde(default)]
    pub browser: Option<BookmarkBrowser>,
    /// Profile to read, by name or directory (default: every profile).
    #[serde(default)]
    pub profile: Option<String>,
    /// A history file read directly: Chromium's `History`, Firefox's
    /// `places.sqlite`, or the profile directory holding one.
    #[serde(default)]
    pub path: Option<String>,
    /// Label of the source in the list (default: the browser and profile).
    #[serde(default)]
    pub name: Option<String>,
    /// Opens this source's pages (default: `addons.browser_history.open_with`,
    /// else the browser and profile they come from).
    #[serde(default)]
    pub open_with: Option<BookmarkOpener>,
}

/// Configuration for the browser history addon (native UI only)
#[derive(Deserialize, JsonSchema, Debug, Clone)]
#[schemars(deny_unknown_fields)]
pub struct BrowserHistoryAddonConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Keyword that opens browser history (default: `hist`).
    #[serde(default)]
    pub trigger: Option<String>,
    /// Opens pages of every source without its own `open_with`
    /// (default: the browser and profile they come from).
    #[serde(default)]
    pub open_with: Option<BookmarkOpener>,
    /// Where history comes from (default: every profile of every supported
    /// browser found).
    #[serde(default)]
    pub sources: Option<Vec<BrowserHistorySourceConfig>>,
    /// Most recent and most visited URLs read per source (default: 2000,
    /// clamped to 100..10000).
    #[serde(default)]
    pub max_entries: Option<usize>,
    /// Close Raffi after opening a page (default: `general.close_on_action`, else true).
    #[serde(default)]
    pub close_on_action: Option<bool>,
}

impl BrowserHistoryAddonConfig {
    pub fn trigger(&self) -> &str {
        self.trigger.as_deref().unwrap_or("hist")
    }

    pub fn max_entries(&self) -> usize {
        self.max_entries.unwrap_or(2000).clamp(100, 10_000)
    }
}

impl Default for BrowserHistoryAddonConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            trigger: None,
            open_with: None,
            sources: None,
            max_entries: None,
            close_on_action: None,
        }
    }
}

/// Configuration for the media controls addon
#[derive(Deserialize, JsonSchema, Debug, Clone)]
#[schemars(deny_unknown_fields)]
pub struct MediaAddonConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Keyword that opens the media controls (default: `media`).
    #[serde(default)]
    pub trigger: Option<String>,
    /// Close Raffi after an action instead of staying open and refreshing
    /// the list (default: `general.close_on_action`, else false).
    #[serde(default)]
    pub close_on_action: Option<bool>,
}

impl MediaAddonConfig {
    pub fn trigger(&self) -> &str {
        self.trigger.as_deref().unwrap_or("media")
    }

    pub fn close_on_action(&self, general: Option<bool>) -> bool {
        self.close_on_action.or(general).unwrap_or(false)
    }
}

impl Default for MediaAddonConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            trigger: None,
            close_on_action: None,
        }
    }
}

/// Configuration for the audio device switcher addon
#[derive(Deserialize, JsonSchema, Debug, Clone)]
#[schemars(deny_unknown_fields)]
pub struct AudioAddonConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Keyword that opens the audio device list (default: `audio`).
    #[serde(default)]
    pub trigger: Option<String>,
    /// Close Raffi after an action instead of staying open and refreshing
    /// the list (default: `general.close_on_action`, else false).
    #[serde(default)]
    pub close_on_action: Option<bool>,
    /// Percent the Volume up and Volume down rows change the output volume
    /// by (default: 5).
    #[serde(default)]
    pub volume_step: Option<u32>,
    /// Highest output volume the volume rows set, in percent (default: 100).
    #[serde(default)]
    pub max_volume: Option<u32>,
}

impl AudioAddonConfig {
    pub fn trigger(&self) -> &str {
        self.trigger.as_deref().unwrap_or("audio")
    }

    pub fn close_on_action(&self, general: Option<bool>) -> bool {
        self.close_on_action.or(general).unwrap_or(false)
    }

    pub fn volume_step(&self) -> u32 {
        self.volume_step.unwrap_or(5).max(1)
    }

    pub fn max_volume(&self) -> u32 {
        self.max_volume.unwrap_or(100)
    }
}

impl Default for AudioAddonConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            trigger: None,
            close_on_action: None,
            volume_step: None,
            max_volume: None,
        }
    }
}

/// Configuration for listing installed applications next to the launchers
#[derive(Deserialize, JsonSchema, Debug, Clone)]
#[schemars(deny_unknown_fields)]
pub struct AppsAddonConfig {
    /// List the installed desktop applications after the launchers
    /// (default: false). Apps named like a launcher, or running the same
    /// command, are left out.
    #[serde(default)]
    pub enabled: bool,
    /// Also list the actions applications declare, such as Firefox's
    /// "New Private Window" (default: true).
    #[serde(default = "default_true")]
    pub actions: bool,
}

impl Default for AppsAddonConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            actions: true,
        }
    }
}

/// Configuration for the run in terminal addon
#[derive(Deserialize, JsonSchema, Debug, Clone)]
#[schemars(deny_unknown_fields)]
pub struct TerminalAddonConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Keyword that runs the rest of the query in a terminal (default: `>`).
    #[serde(default)]
    pub trigger: Option<String>,
    /// Terminal command line the command to run is appended to, such as
    /// `[alacritty, -e]`. Found automatically when unset: xdg-terminal-exec,
    /// then `$TERMINAL`, then the first known terminal installed.
    #[serde(default)]
    pub command: Option<Vec<String>>,
    /// Keep the terminal open once the command exits, until Enter is
    /// pressed (default: true).
    #[serde(default = "default_true")]
    pub hold: bool,
    /// Where typed commands open (default: tab).
    #[serde(default)]
    pub open_in: TerminalOpenIn,
    /// kitty remote control address, such as
    /// `unix:${XDG_RUNTIME_DIR}/kitty`, as given to kitty's `listen_on`.
    /// Read from kitty.conf when unset.
    #[serde(default)]
    pub kitty_socket: Option<String>,
    /// Close Raffi after running a command in the terminal (default: `general.close_on_action`, else true).
    #[serde(default)]
    pub close_on_action: Option<bool>,
}

/// Where the terminal addon opens typed commands
#[derive(Deserialize, JsonSchema, Debug, Clone, Copy, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TerminalOpenIn {
    /// A new tab of the running kitty or WezTerm, else a new window
    #[default]
    Tab,
    /// Always a new window
    Window,
}

impl TerminalAddonConfig {
    pub fn trigger(&self) -> &str {
        self.trigger.as_deref().unwrap_or(">")
    }
}

impl Default for TerminalAddonConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            trigger: None,
            command: None,
            hold: true,
            open_in: TerminalOpenIn::default(),
            kitty_socket: None,
            close_on_action: None,
        }
    }
}

/// Configuration for the kill process addon
#[derive(Deserialize, JsonSchema, Debug, Clone)]
#[schemars(deny_unknown_fields)]
pub struct KillAddonConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Keyword that opens the process list (default: `kill`).
    #[serde(default)]
    pub trigger: Option<String>,
    /// Close Raffi after sending a signal instead of staying open and refreshing
    /// the list (default: `general.close_on_action`, else false).
    #[serde(default)]
    pub close_on_action: Option<bool>,
}

impl KillAddonConfig {
    pub fn trigger(&self) -> &str {
        self.trigger.as_deref().unwrap_or("kill")
    }

    pub fn close_on_action(&self, general: Option<bool>) -> bool {
        self.close_on_action.or(general).unwrap_or(false)
    }
}

impl Default for KillAddonConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            trigger: None,
            close_on_action: None,
        }
    }
}

/// Configuration for the Bluetooth addon
#[derive(Deserialize, JsonSchema, Debug, Clone)]
#[schemars(deny_unknown_fields)]
pub struct BluetoothAddonConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Keyword that opens the Bluetooth list (default: `bt`).
    #[serde(default)]
    pub trigger: Option<String>,
    /// Close Raffi after an action instead of staying open and refreshing
    /// the list (default: `general.close_on_action`, else false).
    #[serde(default)]
    pub close_on_action: Option<bool>,
}

impl BluetoothAddonConfig {
    pub fn trigger(&self) -> &str {
        self.trigger.as_deref().unwrap_or("bt")
    }

    pub fn close_on_action(&self, general: Option<bool>) -> bool {
        self.close_on_action.or(general).unwrap_or(false)
    }
}

impl Default for BluetoothAddonConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            trigger: None,
            close_on_action: None,
        }
    }
}

/// Who asks for the GPG passphrase when a pass entry needs one
#[derive(Deserialize, JsonSchema, Debug, Clone, Copy, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PassPinentry {
    /// Raffi shows its own masked prompt and hands the passphrase to gpg
    /// through loopback pinentry
    #[default]
    Raffi,
    /// Raffi closes and gpg-agent shows its usual pinentry (smartcards,
    /// YubiKeys, or setups that forbid loopback)
    Agent,
}

/// Configuration for the pass (password-store) addon
#[derive(Deserialize, JsonSchema, Debug, Clone)]
#[schemars(deny_unknown_fields)]
pub struct PassAddonConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Keyword that opens the password list (default: `pass`).
    #[serde(default)]
    pub trigger: Option<String>,
    /// Password store directory (default: `$PASSWORD_STORE_DIR`, else
    /// `~/.password-store`).
    #[serde(default)]
    pub store_dir: Option<String>,
    /// Who asks for the GPG passphrase: `raffi` (default) or `agent`.
    #[serde(default)]
    pub pinentry: PassPinentry,
    /// `key: value` lines read as the username, tried in this order (default:
    /// `login`, `user`, `username`). Falls back to the entry file name.
    #[serde(default)]
    pub username_fields: Option<Vec<String>>,
    /// Seconds before a copied secret is cleared from the clipboard
    /// (default: 45, 0 = never).
    #[serde(default)]
    pub clipboard_timeout: Option<u64>,
    /// Milliseconds to wait after Raffi closes before typing (default: 200).
    #[serde(default)]
    pub type_delay_ms: Option<u64>,
    /// Close Raffi after copying a secret (default: `general.close_on_action`, else true).
    #[serde(default)]
    pub close_on_action: Option<bool>,
}

impl PassAddonConfig {
    pub fn trigger(&self) -> &str {
        self.trigger.as_deref().unwrap_or("pass")
    }

    pub fn username_fields(&self) -> Vec<String> {
        self.username_fields
            .clone()
            .unwrap_or_else(|| ["login", "user", "username"].map(String::from).to_vec())
    }

    pub fn clipboard_timeout(&self) -> u64 {
        self.clipboard_timeout.unwrap_or(45)
    }

    pub fn type_delay_ms(&self) -> u64 {
        self.type_delay_ms.unwrap_or(200)
    }
}

impl Default for PassAddonConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            trigger: None,
            store_dir: None,
            pinentry: PassPinentry::default(),
            username_fields: None,
            clipboard_timeout: None,
            type_delay_ms: None,
            close_on_action: None,
        }
    }
}

/// Configuration for the Bitwarden addon, backed by rbw
#[derive(Deserialize, JsonSchema, Debug, Clone)]
#[schemars(deny_unknown_fields)]
pub struct BitwardenAddonConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Keyword that opens the vault list (default: `bw`).
    #[serde(default)]
    pub trigger: Option<String>,
    /// Seconds before a copied secret is cleared from the clipboard
    /// (default: 45, 0 = never).
    #[serde(default)]
    pub clipboard_timeout: Option<u64>,
    /// Milliseconds to wait after Raffi closes before typing (default: 200).
    #[serde(default)]
    pub type_delay_ms: Option<u64>,
    /// Close Raffi after copying a secret (default: `general.close_on_action`, else true).
    #[serde(default)]
    pub close_on_action: Option<bool>,
}

impl BitwardenAddonConfig {
    pub fn trigger(&self) -> &str {
        self.trigger.as_deref().unwrap_or("bw")
    }

    pub fn clipboard_timeout(&self) -> u64 {
        self.clipboard_timeout.unwrap_or(45)
    }

    pub fn type_delay_ms(&self) -> u64 {
        self.type_delay_ms.unwrap_or(200)
    }
}

impl Default for BitwardenAddonConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            trigger: None,
            clipboard_timeout: None,
            type_delay_ms: None,
            close_on_action: None,
        }
    }
}

/// Where the Slack addon opens conversations
#[derive(Deserialize, JsonSchema, Debug, Clone, Copy, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SlackOpenIn {
    /// The Slack desktop app, through `slack://` links
    #[default]
    Desktop,
    /// The browser, through `https://app.slack.com` links
    Browser,
}

/// Order of the messages in the Slack previews
#[derive(Deserialize, JsonSchema, Debug, Clone, Copy, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SlackMessageOrder {
    /// The newest message on top
    #[default]
    NewestFirst,
    /// The oldest message on top, as in Slack
    OldestFirst,
}

/// Configuration for the Slack addon
#[derive(Deserialize, JsonSchema, Debug, Clone, Default)]
#[schemars(deny_unknown_fields)]
pub struct SlackAddonConfig {
    /// Off by default: the addon needs a Slack token.
    #[serde(default)]
    pub enabled: bool,
    /// Keyword that opens the Slack list (default: `slack`).
    #[serde(default)]
    pub trigger: Option<String>,
    /// Shell command printing the Slack token: a user token (`xoxp-`) from
    /// your own Slack app, or the session token (`xoxc-`) of a logged-in
    /// browser tab.
    #[serde(default)]
    pub token_command: Option<String>,
    /// Shell command printing the value of the `d` cookie, needed with a
    /// session (`xoxc-`) token.
    #[serde(default)]
    pub cookie_command: Option<String>,
    /// Open conversations in the desktop app or in the browser (default:
    /// desktop). Ctrl+Enter uses the other one.
    #[serde(default)]
    pub open_in: SlackOpenIn,
    /// Order of the messages in the previews (default: `newest_first`).
    /// Threads keep their first message on top.
    #[serde(default)]
    pub message_order: SlackMessageOrder,
    /// Conversations to check for unread messages (`#channel`, `@user` or
    /// an id), up to 30, used when Slack does not report unread
    /// conversations for the token, as with `xoxp-` tokens.
    #[serde(default)]
    pub unread_conversations: Vec<String>,
    /// Minutes the cached conversations, people, muted conversations and
    /// searches count as fresh before Raffi asks Slack again (default: 60);
    /// 0 asks every time. Cached rows and previews show at once while the
    /// fresh ones load.
    #[serde(default)]
    pub cache_minutes: Option<u64>,
    /// Close Raffi after opening a conversation or message (default: `general.close_on_action`, else false).
    #[serde(default)]
    pub close_on_action: Option<bool>,
}

impl SlackAddonConfig {
    pub fn trigger(&self) -> &str {
        self.trigger.as_deref().unwrap_or("slack")
    }

    pub fn close_on_action(&self, general: Option<bool>) -> bool {
        self.close_on_action.or(general).unwrap_or(false)
    }

    pub fn cache_fresh(&self) -> std::time::Duration {
        self.cache_minutes
            .map(|minutes| std::time::Duration::from_secs(minutes.saturating_mul(60)))
            .unwrap_or(std::time::Duration::from_secs(60 * 60))
    }
}

/// Container for all addon configurations
#[derive(Deserialize, JsonSchema, Debug, Clone, Default)]
#[schemars(deny_unknown_fields)]
pub struct AddonsConfig {
    #[serde(default)]
    pub currency: CurrencyAddonConfig,
    #[serde(default)]
    pub calculator: CalculatorAddonConfig,
    #[serde(default)]
    pub units: UnitsAddonConfig,
    #[serde(default)]
    pub color: ColorAddonConfig,
    #[serde(default)]
    pub world_clock: WorldClockAddonConfig,
    #[serde(default)]
    pub file_browser: FileBrowserAddonConfig,
    #[serde(default)]
    pub emoji: EmojiAddonConfig,
    #[serde(default)]
    pub clipboard: ClipboardAddonConfig,
    #[serde(default)]
    pub system: SystemAddonConfig,
    #[serde(default)]
    pub windows: WindowsAddonConfig,
    #[serde(default)]
    pub browser_tabs: BrowserTabsAddonConfig,
    #[serde(default)]
    pub bookmarks: BookmarksAddonConfig,
    #[serde(default)]
    pub browser_history: BrowserHistoryAddonConfig,
    #[serde(default)]
    pub media: MediaAddonConfig,
    #[serde(default)]
    pub audio: AudioAddonConfig,
    #[serde(default)]
    pub apps: AppsAddonConfig,
    #[serde(default)]
    pub terminal: TerminalAddonConfig,
    #[serde(default)]
    pub kill: KillAddonConfig,
    #[serde(default)]
    pub bluetooth: BluetoothAddonConfig,
    #[serde(default)]
    pub pass: PassAddonConfig,
    #[serde(default)]
    pub bitwarden: BitwardenAddonConfig,
    #[serde(default)]
    pub slack: SlackAddonConfig,
    #[serde(default)]
    pub devtools: DevtoolsAddonConfig,
    #[serde(default)]
    pub script_filters: Vec<ScriptFilterConfig>,
    #[serde(default)]
    pub web_searches: Vec<WebSearchConfig>,
    #[serde(default)]
    pub text_snippets: Vec<TextSnippetSourceConfig>,
    /// Characters to type before addon keywords are suggested (default: 3).
    /// A keyword typed in full is always suggested.
    #[serde(default)]
    pub keyword_min_chars: Option<usize>,
    /// Built from the launcher entries, not read from this section.
    #[serde(skip)]
    #[schemars(skip)]
    pub launcher_keywords: Vec<LauncherKeyword>,
}

impl AddonsConfig {
    pub fn keyword_min_chars(&self) -> usize {
        self.keyword_min_chars.unwrap_or(3)
    }
}

fn default_true() -> bool {
    true
}

/// Per-colour overrides for the theme.
#[derive(Deserialize, JsonSchema, Debug, Clone, Default)]
#[schemars(deny_unknown_fields)]
pub struct ThemeColorsConfig {
    pub bg_base: Option<String>,
    pub bg_input: Option<String>,
    pub accent: Option<String>,
    pub accent_hover: Option<String>,
    pub text_main: Option<String>,
    pub text_muted: Option<String>,
    pub selection_bg: Option<String>,
    pub border: Option<String>,
}

/// Colour overrides for the right-hand preview pane (hex: #RGB, #RRGGBB, or #RRGGBBAA).
#[derive(Deserialize, JsonSchema, Debug, Clone, Default, PartialEq)]
#[schemars(deny_unknown_fields)]
pub struct PreviewColorsConfig {
    /// Background of the whole preview pane
    pub bg: Option<String>,
    /// Background of the content/code blocks inside the pane
    pub bg_block: Option<String>,
    /// Title and main text colour
    pub text_main: Option<String>,
    /// Subtitles, metadata keys and hints
    pub text_muted: Option<String>,
    /// Headings, badges and hero values
    pub accent: Option<String>,
    /// Pane and block borders
    pub border: Option<String>,
}

/// Appearance settings for the right-hand preview pane.
#[derive(Deserialize, JsonSchema, Debug, Clone, Default, PartialEq)]
#[schemars(deny_unknown_fields)]
pub struct PreviewConfig {
    /// Base font size in px for preview text (default: 0.8 × general font_size,
    /// 16 at the default 20). Title, headings and hero values scale from it.
    #[serde(default)]
    pub font_size: Option<f32>,
    /// Font family for the preview pane, including message, snippet and
    /// clipboard text (default: the UI font)
    #[serde(default)]
    pub font_family: Option<String>,
    /// Monospace font family for code blocks such as file contents, commands
    /// and script output (default: system monospace)
    #[serde(default)]
    pub mono_font_family: Option<String>,
    /// Width of the preview pane as a percentage of the window (10-80, default: 50)
    #[serde(default)]
    pub width_percent: Option<f32>,
    /// Colour overrides for the preview pane
    #[serde(default)]
    pub colors: Option<PreviewColorsConfig>,
}

/// Sort mode for launcher entries, text snippets and password entries by past use
#[derive(Deserialize, JsonSchema, Debug, Clone, Default, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum SortMode {
    /// Sort by launch count (most frequently used first)
    Frequency,
    /// Sort by last-used time (most recently used first)
    Recency,
    /// Combined score: 40% frequency + 60% recency
    #[default]
    Hybrid,
}

/// What the chips on an empty main view query show
#[derive(Deserialize, JsonSchema, Debug, Clone, Copy, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MainViewChips {
    /// A short name such as "Tabs" or "Clipboard"
    #[default]
    Name,
    /// The keyword that starts the plugin, such as `tabs` or `clip`
    Keyword,
    /// No chips: an empty query starts with the apps
    Off,
}

/// How Raffi draws its window
#[derive(Deserialize, JsonSchema, Debug, Clone, Copy, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Renderer {
    /// `gpu` when `opacity` is below 1, since only the GPU renderer shows
    /// transparency on Wayland, `cpu` otherwise
    #[default]
    Auto,
    /// Software rendering: opens faster because no GPU driver has to be loaded.
    /// The window stays opaque on Wayland.
    Cpu,
    /// GPU rendering through Vulkan, falling back to software if no GPU is usable
    Gpu,
}

/// Clipboard manager used for `{clipboard:N}` snippet placeholders
#[derive(Deserialize, JsonSchema, Debug, Clone, Copy, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ClipboardHistory {
    /// Use whichever of cliphist, CopyQ or clipman is running
    #[default]
    Auto,
    /// Read history with `cliphist list` and `cliphist decode`
    Cliphist,
    /// Read history with `copyq read`
    Copyq,
    /// Read clipman's JSON history file
    Clipman,
    /// Disable clipboard history placeholders
    None,
}

/// General configuration for persistent defaults
#[derive(Deserialize, JsonSchema, Debug, Clone, Default)]
#[schemars(deny_unknown_fields)]
pub struct GeneralConfig {
    #[serde(default)]
    pub ui_type: Option<String>,
    #[serde(default)]
    pub default_script_shell: Option<String>,
    #[serde(default)]
    pub no_icons: Option<bool>,
    #[serde(default)]
    pub theme: Option<String>,
    #[serde(default)]
    pub theme_colors: Option<ThemeColorsConfig>,
    /// Opacity of the window backgrounds, from 0.0 (see-through) to 1.0
    /// (the theme as designed). Text and icons stay opaque. Default: 0.95.
    #[serde(default)]
    #[schemars(range(min = 0.0, max = 1.0))]
    pub opacity: Option<f32>,
    /// Ask the compositor to blur what shows through the window
    /// (KDE/KWin; other compositors use their own rules). Default: true.
    #[serde(default)]
    pub blur: Option<bool>,
    /// Renderer for the window: `auto` (default: `gpu` when `opacity` is
    /// below 1, else `cpu`), `cpu` (faster to open, opaque on Wayland) or `gpu`.
    /// An `ICED_BACKEND` environment variable takes precedence.
    #[serde(default)]
    pub renderer: Renderer,
    #[serde(default)]
    pub max_history: Option<u32>,
    #[serde(default)]
    pub font_size: Option<f32>,
    #[serde(default)]
    pub font_family: Option<String>,
    #[serde(default)]
    pub window_width: Option<f32>,
    #[serde(default)]
    pub window_height: Option<f32>,
    #[serde(default)]
    pub padding: Option<f32>,
    #[serde(default)]
    pub sort_mode: SortMode,
    #[serde(default)]
    pub fallbacks: Vec<String>,
    /// Sources mixed into the main view, in display order, when typing without
    /// an addon trigger. Entries: `launchers`, `browser_tabs`,
    /// `bookmarks`, `browser_history`, `windows`, `system`, `media`, `audio`,
    /// `bluetooth`, `pass`,
    /// `kill`, `slack`, `slack.unread`, `slack.threads` (`slack.unread_threads`),
    /// `slack.activity`, `slack.actions`, `slack.directs` (`slack.dms`),
    /// `slack.channels`, `text_snippets.<name>` and `script_filters.<name>`.
    /// Slack shows its Home rows on an empty query and searches messages when
    /// typing. When set, launchers only show if `launchers` is listed.
    #[serde(default)]
    pub main_view: Option<Vec<String>>,
    /// What the chips row above the apps shows on an empty query when
    /// `main_view` is set: `name` (default), `keyword` or `off`
    #[serde(default)]
    pub main_view_chips: MainViewChips,
    /// Clipboard manager for `{clipboard:N}` snippet placeholders
    #[serde(default)]
    pub clipboard_history: ClipboardHistory,
    /// Extra window classes treated as terminals by `hypr_paste` (sent Ctrl+Shift+V
    /// instead of Ctrl+V). Added to the built-in list; matched case-insensitively.
    #[serde(default)]
    pub terminal_classes: Vec<String>,
    /// Show the Alfred-style right-hand preview pane.
    /// By default, previews are shown automatically for addons (snippets, file browser,
    /// script filters, etc.) and hidden for standard launcher entries unless `show_preview: true`
    /// is set in `general` or on the launcher entry. Can also be toggled at runtime with Ctrl+P.
    #[serde(default)]
    pub show_preview: Option<bool>,
    /// Show a tip about a keyword in the empty search box, such as
    /// `clip for Clipboard history` (default: true).
    #[serde(default)]
    pub show_tips: Option<bool>,
    /// Close Raffi after executing an action (default: true).
    /// Addon and launcher `close_on_action` settings override this.
    #[serde(default)]
    pub close_on_action: Option<bool>,
    /// Appearance of the right-hand preview pane (font, size, colours, width)
    #[serde(default)]
    pub preview: PreviewConfig,
}

/// Main view sources used when there is no config file. They need no setup,
/// and each one hides itself when its tool or compositor is missing.
#[cfg(feature = "wayland")]
const FIRST_RUN_MAIN_VIEW: &[&str] = &[
    "launchers",
    "windows",
    "browser_tabs",
    "bookmarks",
    "system",
    "media",
    "audio",
    "bluetooth",
    "pass",
    "clipboard",
    "bitwarden",
    "terminal",
    "emoji",
];

/// General settings used when there is no config file at all.
fn first_run_general() -> GeneralConfig {
    GeneralConfig {
        #[cfg(feature = "wayland")]
        main_view: Some(
            FIRST_RUN_MAIN_VIEW
                .iter()
                .map(|name| name.to_string())
                .collect(),
        ),
        ..GeneralConfig::default()
    }
}

/// Complete parsed configuration
#[derive(Debug)]
pub struct ParsedConfig {
    pub general: GeneralConfig,
    pub addons: AddonsConfig,
    pub entries: Vec<RaffiConfig>,
}

/// Represents the top-level configuration structure (v1 format).
#[derive(Deserialize, Default)]
struct Config {
    #[serde(default)]
    #[allow(dead_code)]
    version: u32,
    #[serde(default)]
    general: GeneralConfig,
    #[serde(default)]
    addons: AddonsConfig,
    /// A YAML mapping rather than a HashMap so launchers keep their file
    /// order, which breaks ties in the usage sort.
    #[serde(default)]
    launchers: serde_yaml::Mapping,
}

/// Public schema representation of the v1 config format, used for JSON Schema generation.
#[derive(JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ConfigSchema {
    /// Config format version (currently 1)
    pub version: u32,
    /// General settings (UI, theme, font, etc.)
    #[serde(default)]
    pub general: GeneralConfig,
    /// Addon configurations (script filters, web searches, etc.)
    #[serde(default)]
    pub addons: AddonsConfig,
    /// Launcher entries keyed by name
    #[serde(default)]
    pub launchers: HashMap<String, RaffiConfig>,
}

/// UI type selection
#[derive(Debug, Clone, PartialEq)]
pub enum UIType {
    Fuzzel,
    #[cfg(feature = "wayland")]
    Native,
}

/// Theme mode selection
#[derive(Debug, Clone, PartialEq)]
pub enum ThemeMode {
    Dark,
    Light,
    CatppuccinMocha,
    CatppuccinLatte,
    TokyoNight,
    Nord,
    Gruvbox,
    Dracula,
    RosePine,
    Kanagawa,
}

impl ThemeMode {
    /// Returns true if this preset is a light theme.
    pub fn is_light(&self) -> bool {
        matches!(self, Self::Light | Self::CatppuccinLatte)
    }
}

impl std::str::FromStr for ThemeMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_lowercase().replace('_', "-").as_str() {
            "dark" | "obsidian" => Ok(ThemeMode::Dark),
            "light" | "rose-pine-dawn" | "rosepinedawn" => Ok(ThemeMode::Light),
            "catppuccin" | "catppuccin-mocha" => Ok(ThemeMode::CatppuccinMocha),
            "catppuccin-latte" => Ok(ThemeMode::CatppuccinLatte),
            "tokyo-night" | "tokyonight" => Ok(ThemeMode::TokyoNight),
            "nord" => Ok(ThemeMode::Nord),
            "gruvbox" | "gruvbox-dark" => Ok(ThemeMode::Gruvbox),
            "dracula" => Ok(ThemeMode::Dracula),
            "rose-pine" | "rosepine" => Ok(ThemeMode::RosePine),
            "kanagawa" => Ok(ThemeMode::Kanagawa),
            _ => Err(format!(
                "Invalid theme: {}. Valid options are: dark, light, catppuccin, catppuccin-latte, tokyo-night, nord, gruvbox, dracula, rose-pine, kanagawa",
                s
            )),
        }
    }
}

impl std::str::FromStr for UIType {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "fuzzel" => Ok(UIType::Fuzzel),
            #[cfg(feature = "wayland")]
            "native" | "wayland" | "iced" => Ok(UIType::Native),
            #[cfg(not(feature = "wayland"))]
            "native" | "wayland" | "iced" => Err(
                "This build only supports Fuzzel. Build with the 'wayland' feature for Raffi's own window."
                    .to_string(),
            ),
            _ => {
                #[cfg(feature = "wayland")]
                {
                    Err(format!(
                        "Invalid UI type: {}. Use fuzzel, or leave it unset for Raffi's own window",
                        s
                    ))
                }
                #[cfg(not(feature = "wayland"))]
                {
                    Err(format!("Invalid UI type: {}. Valid options are: fuzzel", s))
                }
            }
        }
    }
}

/// Command-line arguments structure.
#[derive(Debug, Options, Clone, Default)]
pub struct Args {
    #[options(help = "print help message")]
    pub help: bool,
    #[options(help = "print version")]
    pub version: bool,
    #[options(help = "config file location", short = "c")]
    pub configfile: Option<String>,
    #[options(help = "print command to stdout, do not run it", short = "p")]
    pub print_only: bool,
    #[options(help = "refresh cache", short = "r")]
    pub refresh_cache: bool,
    #[options(help = "do not show icons", short = "I")]
    pub no_icons: bool,
    #[options(help = "default shell when using scripts", short = "P")]
    pub default_script_shell: Option<String>,
    #[options(
        help = "show entries in fuzzel instead of Raffi's own window",
        short = "u"
    )]
    pub ui_type: Option<String>,
    #[options(help = "application ID for the Raffi window on Linux", no_short)]
    pub app_name: Option<String>,
    #[options(help = "initial search query", short = "i")]
    pub initial_query: Option<String>,
    #[options(
        help = "activate the only selectable result for a non-empty initial query",
        short = "1"
    )]
    pub select_1: bool,
    #[options(
        help = "theme: dark, light, catppuccin, catppuccin-latte, tokyo-night, nord, gruvbox, dracula, rose-pine, kanagawa (default: dark)",
        short = "t"
    )]
    pub theme: Option<String>,
    #[options(
        help = "window background opacity, 0.0 to 1.0 (default: 0.95)",
        no_short,
        meta = "OPACITY"
    )]
    pub opacity: Option<f32>,
    #[options(
        help = "keep the window open after executing an action (shorthand for -o general.close_on_action=false)",
        no_short
    )]
    pub keep_open: bool,
    #[options(
        help = "override a configuration setting (repeatable, e.g. -o general.close_on_action=false)",
        short = "o",
        long = "option",
        meta = "KEY=VALUE"
    )]
    pub options: Vec<String>,
    #[options(
        help = "write both JSON Schemas beside the config file and exit",
        no_short
    )]
    pub generate_schemas: bool,
    #[options(
        help = "validate the config file against the JSON Schema and exit",
        no_short
    )]
    pub check_config: bool,
    #[options(help = "write debug log to FILE")]
    pub debug_file: Option<String>,
    #[options(
        help = "register Raffi as the native host of the Vicinae browser extension and exit",
        no_short
    )]
    pub install_browser_host: bool,
    #[options(
        help = "remove the browser host manifests pointing to Raffi and exit",
        no_short
    )]
    pub uninstall_browser_host: bool,
    #[options(
        help = "with --install-browser-host, replace manifests of other hosts such as Vicinae",
        no_short
    )]
    pub force: bool,
}

fn resolve_window_dimensions(general: &GeneralConfig) -> (f32, f32) {
    (
        general.window_width.unwrap_or(800.0),
        general.window_height.unwrap_or(600.0),
    )
}

const DEFAULT_OPACITY: f32 = 0.95;

/// Background opacity for the window: `--opacity` wins over `general.opacity`.
fn resolve_opacity(args: &Args, general: &GeneralConfig) -> f32 {
    args.opacity
        .or(general.opacity)
        .filter(|o| o.is_finite())
        .unwrap_or(DEFAULT_OPACITY)
        .clamp(0.0, 1.0)
}

fn resolve_renderer(renderer: Renderer, opacity: f32) -> Renderer {
    match renderer {
        Renderer::Auto if opacity < 1.0 => Renderer::Gpu,
        Renderer::Auto => Renderer::Cpu,
        explicit => explicit,
    }
}

/// A trait for checking environment variables.
pub trait EnvProvider {
    fn var(&self, key: &str) -> Result<String, std::env::VarError>;
}

/// The default environment provider.
pub struct DefaultEnvProvider;

impl EnvProvider for DefaultEnvProvider {
    fn var(&self, key: &str) -> Result<String, std::env::VarError> {
        std::env::var(key)
    }
}

/// A trait for checking if a binary exists.
pub trait BinaryChecker {
    fn exists(&self, binary: &str) -> bool;
}

/// The default binary checker.
pub struct DefaultBinaryChecker;

impl BinaryChecker for DefaultBinaryChecker {
    fn exists(&self, binary: &str) -> bool {
        find_binary(binary)
    }
}

impl BinaryChecker for fn(&str) -> bool {
    fn exists(&self, binary: &str) -> bool {
        self(binary)
    }
}

/// A trait for providing an icon map.
pub trait IconMapProvider {
    fn get_icon_map(&self) -> Result<HashMap<String, String>>;
}

/// The default icon map provider.
pub struct DefaultIconMapProvider;

impl IconMapProvider for DefaultIconMapProvider {
    fn get_icon_map(&self) -> Result<HashMap<String, String>> {
        read_icon_map()
    }
}

/// Extract icon size from path (e.g., "/usr/share/icons/Papirus/48x48/apps/icon.svg" -> 48).
/// Returns 0 if size cannot be determined.
fn extract_icon_size(path: &std::path::Path) -> u32 {
    for component in path.components() {
        if let std::path::Component::Normal(s) = component {
            if let Some(s_str) = s.to_str() {
                // Match patterns like "48x48", "64x64", "scalable"
                if s_str == "scalable" {
                    return 512; // Treat scalable as large
                }
                if let Some((w, _)) = s_str.split_once('x') {
                    if let Ok(size) = w.parse::<u32>() {
                        return size;
                    }
                }
            }
        }
    }
    0
}

/// Get the icon mapping from system directories.
/// Prefers larger icons (48x48+) since raffi renders at 48x48.
fn get_icon_map() -> Result<HashMap<String, String>> {
    let mut icon_map: HashMap<String, String> = HashMap::new();
    let mut icon_sizes: HashMap<String, u32> = HashMap::new();
    let mut data_dirs =
        std::env::var("XDG_DATA_DIRS").unwrap_or("/usr/local/share/:/usr/share/".to_string());
    let data_home = std::env::var("XDG_DATA_HOME").unwrap_or_else(|_| {
        format!(
            "{}/.local/share/",
            std::env::var("HOME").unwrap_or_default()
        )
    });
    write!(&mut data_dirs, ":{data_home}")?;

    for datadir in std::env::split_paths(&data_dirs) {
        for subdir in &["icons", "pixmaps"] {
            let mut dir = datadir.clone();
            dir.push(subdir);
            for entry in walkdir::WalkDir::new(dir)
                .follow_links(true)
                .into_iter()
                .filter_map(Result::ok)
            {
                let fname = entry.file_name().to_string_lossy().to_string();
                if let Some(ext) = entry.path().extension().and_then(|s| s.to_str()) {
                    if ext == "png" || ext == "svg" {
                        let icon_name = fname.rsplit_once('.').unwrap().0.to_string();
                        let icon_path = entry.path().to_string_lossy().to_string();
                        let new_size = extract_icon_size(entry.path());
                        let current_size = icon_sizes.get(&icon_name).copied().unwrap_or(0);

                        // Prefer icons >= 48px, or larger than current
                        if new_size >= current_size || (new_size >= 48 && current_size < 48) {
                            icon_map.insert(icon_name.clone(), icon_path);
                            icon_sizes.insert(icon_name, new_size);
                        }
                    }
                }
            }
        }
    }
    Ok(icon_map)
}

/// Expand `~/` prefix to the user's HOME directory.
pub(crate) fn expand_tilde(s: &str) -> String {
    if let Some(stripped) = s.strip_prefix("~/") {
        format!("{}/{}", std::env::var("HOME").unwrap_or_default(), stripped)
    } else {
        s.to_string()
    }
}

/// Expand `${VAR}` references to their environment variable values.
/// Unknown or unset variables expand to an empty string.
fn expand_env_vars(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '$' && chars.peek() == Some(&'{') {
            chars.next(); // consume '{'
            let var_name: String = chars.by_ref().take_while(|&c| c != '}').collect();
            result.push_str(&std::env::var(&var_name).unwrap_or_default());
        } else {
            result.push(c);
        }
    }
    result
}

/// Expand both `~/` and `${VAR}` in a config value.
pub(crate) fn expand_config_value(s: &str) -> String {
    expand_env_vars(&expand_tilde(s))
}

/// Migrate a v0 config to the v1 shape in memory, without touching any file.
/// Returns `true` if migration was performed, `false` if already v1+.
fn migrate_config_v0_in_memory(raw: &mut Value) -> bool {
    let mapping = match raw.as_mapping_mut() {
        Some(m) => m,
        None => return false,
    };

    // If version key exists, no migration needed
    if mapping.contains_key(Value::String("version".to_string())) {
        return false;
    }

    let reserved = ["general", "addons", "version"];
    let mut launchers = serde_yaml::Mapping::new();
    let mut keys_to_move = Vec::new();

    for (key, _) in mapping.iter() {
        if let Some(k) = key.as_str() {
            if !reserved.contains(&k) {
                keys_to_move.push(Value::String(k.to_string()));
            }
        }
    }

    for key in &keys_to_move {
        if let Some(val) = mapping.remove(key) {
            launchers.insert(key.clone(), val);
        }
    }

    mapping.insert(
        Value::String("version".to_string()),
        Value::Number(serde_yaml::Number::from(1u64)),
    );
    mapping.insert(
        Value::String("launchers".to_string()),
        Value::Mapping(launchers),
    );

    true
}

/// Migrate a v0 config (launcher entries as top-level keys) to v1 format
/// (entries under a `launchers` key with an explicit `version` field).
/// Returns `Ok(true)` if migration was performed, `Ok(false)` if already v1+.
pub fn migrate_config_v0_to_v1(config_path: &str, raw: &mut Value) -> Result<bool> {
    if !migrate_config_v0_in_memory(raw) {
        return Ok(false);
    }

    // Back up original file
    let backup_path = format!("{config_path}.bak");
    fs::copy(config_path, &backup_path)
        .context(format!("Failed to create backup at {backup_path}"))?;

    // Write migrated config
    let migrated_yaml =
        serde_yaml::to_string(&raw).context("Failed to serialize migrated config")?;
    fs::write(config_path, &migrated_yaml)
        .context(format!("Failed to write migrated config to {config_path}"))?;

    eprintln!("Config migrated to v1 format. Backup saved as {backup_path}");
    Ok(true)
}

/// A single problem found while checking a config.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigProblem {
    /// JSON pointer to the offending value, `/` for the document root.
    pub path: String,
    /// Offending key for unknown-property errors, used to point at its line.
    pub key: Option<String>,
    pub message: String,
    /// 1-based line in the config file, when it could be located.
    pub line: Option<usize>,
}

impl ConfigProblem {
    fn new(path: &str, message: impl Into<String>) -> Self {
        Self {
            path: if path.is_empty() { "/" } else { path }.to_string(),
            key: None,
            message: message.into(),
            line: None,
        }
    }
}

impl std::fmt::Display for ConfigProblem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.path, self.message)
    }
}

/// Validate a v1-shaped config against the generated JSON Schema.
/// Returns one problem per schema violation; empty means valid.
#[cfg(feature = "config-validation")]
pub fn validate_config_value(raw: &Value) -> Vec<ConfigProblem> {
    let instance = match serde_json::to_value(raw) {
        Ok(v) => v,
        Err(e) => {
            return vec![ConfigProblem::new(
                "/",
                format!("cannot check config against the schema: {e}"),
            )]
        }
    };
    let schema = match serde_json::to_value(schemars::schema_for!(ConfigSchema)) {
        Ok(v) => v,
        Err(e) => {
            return vec![ConfigProblem::new(
                "/",
                format!("cannot serialize the built-in schema: {e}"),
            )]
        }
    };
    let validator = match jsonschema::validator_for(&schema) {
        Ok(v) => v,
        Err(e) => {
            return vec![ConfigProblem::new(
                "/",
                format!("cannot compile the built-in schema: {e}"),
            )]
        }
    };
    validator
        .iter_errors(&instance)
        .map(|e| {
            let mut problem = ConfigProblem::new(e.instance_path().as_str(), e.to_string());
            if let jsonschema::error::ValidationErrorKind::AdditionalProperties { unexpected } =
                e.kind()
            {
                problem.key = unexpected.first().cloned();
            }
            problem
        })
        .collect()
}

#[cfg(not(feature = "config-validation"))]
pub fn validate_config_value(_raw: &Value) -> Vec<ConfigProblem> {
    Vec::new()
}

enum YamlTokenKind {
    SeqItem,
    Key(String),
    Other,
}

struct YamlToken {
    line: usize,
    indent: usize,
    kind: YamlTokenKind,
}

fn parse_yaml_key(s: &str) -> Option<String> {
    let (key, after) = match s.chars().next()? {
        q @ ('"' | '\'') => {
            let end = s[1..].find(q)? + 1;
            (&s[1..end], &s[end + 1..])
        }
        '{' | '[' | '?' | '&' | '*' | '!' | '|' | '>' => return None,
        _ => {
            let end = s.find(": ").or_else(|| s.strip_suffix(':').map(str::len))?;
            (s[..end].trim_end(), &s[end..])
        }
    };
    after.starts_with(':').then(|| key.to_string())
}

/// Split block-style YAML into key and sequence-item tokens with their column.
/// `  - name: x` yields a sequence item at column 2 and a key at column 4.
fn tokenize_yaml(text: &str) -> Vec<YamlToken> {
    let mut tokens = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let mut rest = raw.trim_start_matches(' ');
        let mut indent = raw.len() - rest.len();
        if rest.is_empty() || rest.starts_with('#') || rest.starts_with("---") {
            continue;
        }
        while rest == "-" || rest.starts_with("- ") {
            tokens.push(YamlToken {
                line: i + 1,
                indent,
                kind: YamlTokenKind::SeqItem,
            });
            let after = rest[1..].trim_start_matches(' ');
            indent += rest.len() - after.len();
            rest = after;
        }
        if rest.is_empty() || rest.starts_with('#') {
            continue;
        }
        tokens.push(YamlToken {
            line: i + 1,
            indent,
            kind: parse_yaml_key(rest).map_or(YamlTokenKind::Other, YamlTokenKind::Key),
        });
    }
    tokens
}

/// Best-effort lookup of the 1-based line a JSON pointer refers to in
/// block-style YAML. Falls back to the deepest ancestor that could be found.
fn find_yaml_line(text: &str, pointer: &str) -> Option<usize> {
    let tokens = tokenize_yaml(text);
    let (mut start, mut end) = (0, tokens.len());
    let mut found = None;
    for segment in pointer.split('/').skip(1).filter(|s| !s.is_empty()) {
        let segment = segment.replace("~1", "/").replace("~0", "~");
        let Some(level) = tokens[start..end].first().map(|t| t.indent) else {
            break;
        };
        let children = || (start..end).filter(|&j| tokens[j].indent == level);
        let hit = children()
            .find(|&j| matches!(&tokens[j].kind, YamlTokenKind::Key(k) if *k == segment))
            .or_else(|| {
                let n = segment.parse::<usize>().ok()?;
                children()
                    .filter(|&j| matches!(tokens[j].kind, YamlTokenKind::SeqItem))
                    .nth(n)
            });
        let Some(j) = hit else {
            break;
        };
        let parent = &tokens[j];
        found = Some(parent.line);
        start = j + 1;
        // A key's block may hold sequence items at the key's own column.
        end = (start..end)
            .find(|&k| {
                let t = &tokens[k];
                t.indent < parent.indent
                    || (t.indent == parent.indent
                        && !(matches!(parent.kind, YamlTokenKind::Key(_))
                            && matches!(t.kind, YamlTokenKind::SeqItem)))
            })
            .unwrap_or(end);
    }
    found
}

/// Fill in `line` for each problem by locating its path in the YAML `text`.
/// `v0` means `text` uses the old flat format, where launchers sit at the top level.
fn locate_config_problems(problems: &mut [ConfigProblem], text: &str, v0: bool) {
    for problem in problems {
        let mut pointer = problem.path.clone();
        if let Some(key) = &problem.key {
            pointer = format!(
                "{}/{}",
                pointer.trim_end_matches('/'),
                key.replace('~', "~0").replace('/', "~1")
            );
        }
        if v0 {
            if let Some(rest) = pointer.strip_prefix("/launchers") {
                if rest.is_empty() || rest.starts_with('/') {
                    pointer = rest.to_string();
                }
            }
        }
        problem.line = find_yaml_line(text, &pointer);
    }
}

/// Format a problem as `source:LINE: path: message`, dropping `LINE` when unknown.
fn format_config_problem(source: &str, problem: &ConfigProblem) -> String {
    match problem.line {
        Some(line) => format!("{source}:{line}: {problem}"),
        None => format!("{source}: {problem}"),
    }
}

fn report_config_warnings(raw: &Value, text: &str, v0: bool, source: &str) {
    let mut problems = validate_config_value(raw);
    locate_config_problems(&mut problems, text, v0);
    print_config_warnings(&problems, source);
}

fn print_config_warnings(problems: &[ConfigProblem], source: &str) {
    for problem in problems {
        let problem = format_config_problem(source, problem);
        debug_log!("config warning: {problem}");
        eprintln!("raffi: config warning: {problem}");
    }
}

fn ensure_config_validation() -> Result<()> {
    if !cfg!(feature = "config-validation") {
        anyhow::bail!(
            "config validation is not available in this build; rebuild with the `config-validation` feature"
        );
    }
    Ok(())
}

/// Check a config file without running raffi or rewriting the file.
/// Returns the list of problems found; empty means the config is valid.
pub fn check_config(filename: &str) -> Result<Vec<ConfigProblem>> {
    ensure_config_validation()?;
    let contents =
        fs::read_to_string(filename).context(format!("cannot open config file {filename}"))?;
    let mut raw: Value = serde_yaml::from_str(&contents).context("cannot parse config as YAML")?;
    let v0 = migrate_config_v0_in_memory(&mut raw);

    let mut problems = validate_config_value(&raw);
    locate_config_problems(&mut problems, &contents, v0);
    if problems.is_empty() {
        // Parse v1 files from the text so serde can report where it failed.
        let result = if v0 {
            serde_yaml::from_value::<Config>(raw)
        } else {
            serde_yaml::from_str::<Config>(&contents)
        };
        if let Err(e) = result {
            let mut problem = ConfigProblem::new("/", e.to_string());
            problem.line = e.location().map(|l| l.line());
            problems.push(problem);
        }
    }
    Ok(problems)
}

/// Split a dot-separated CLI override key into path segments, respecting
/// quoted or bracketed segments such as `addons.script_filters."My Filter".close_on_action`
/// or `addons.script_filters[My Filter].close_on_action`.
fn parse_override_path(key: &str) -> Result<Vec<String>> {
    let key = key.trim();
    if key.is_empty() {
        anyhow::bail!("empty configuration key");
    }
    let mut segments = Vec::new();
    let mut current = String::new();
    let mut chars = key.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' | '\'' => {
                let quote = c;
                let mut closed = false;
                for next in chars.by_ref() {
                    if next == quote {
                        closed = true;
                        break;
                    }
                    current.push(next);
                }
                if !closed {
                    anyhow::bail!("unterminated quote in configuration key '{key}'");
                }
            }
            '[' => {
                if !current.is_empty() {
                    segments.push(std::mem::take(&mut current));
                }
                let mut bracket = String::new();
                let mut closed = false;
                for next in chars.by_ref() {
                    if next == ']' {
                        closed = true;
                        break;
                    }
                    bracket.push(next);
                }
                if !closed {
                    anyhow::bail!("unterminated '[' in configuration key '{key}'");
                }
                let trimmed = bracket
                    .trim()
                    .trim_matches('"')
                    .trim_matches('\'')
                    .to_string();
                if trimmed.is_empty() {
                    anyhow::bail!("empty '[]' in configuration key '{key}'");
                }
                segments.push(trimmed);
                if chars.peek() == Some(&'.') {
                    chars.next();
                }
            }
            '.' => {
                if current.is_empty() {
                    anyhow::bail!("empty segment in configuration key '{key}'");
                }
                segments.push(std::mem::take(&mut current));
            }
            _ => current.push(c),
        }
    }
    if !current.is_empty() {
        segments.push(current);
    }
    if segments.is_empty() {
        anyhow::bail!("empty configuration key");
    }
    Ok(segments)
}

/// Set a single `KEY=VALUE` override in `raw`.
fn set_yaml_override(raw: &mut Value, spec: &str) -> Result<()> {
    let (key, val_str) = spec
        .split_once('=')
        .ok_or_else(|| anyhow::anyhow!("invalid option '{spec}': expected KEY=VALUE"))?;
    let segments = parse_override_path(key)?;
    let parsed_val: Value = serde_yaml::from_str(val_str)
        .with_context(|| format!("invalid YAML value in option '{spec}'"))?;

    if !raw.is_mapping() {
        let mut root = serde_yaml::Mapping::new();
        root.insert(Value::String("version".to_string()), Value::from(1u64));
        *raw = Value::Mapping(root);
    }
    if let Some(root) = raw.as_mapping_mut() {
        root.entry(Value::String("version".to_string()))
            .or_insert_with(|| Value::from(1u64));
    }

    let mut cursor = raw;
    for (i, segment) in segments.iter().enumerate() {
        let is_last = i + 1 == segments.len();
        if is_last {
            match cursor {
                Value::Mapping(map) => {
                    map.insert(Value::String(segment.clone()), parsed_val);
                    return Ok(());
                }
                Value::Sequence(seq) => {
                    if let Ok(index) = segment.parse::<usize>() {
                        let len = seq.len();
                        let slot = seq.get_mut(index).ok_or_else(|| {
                            anyhow::anyhow!(
                                "index {index} out of bounds in option '{spec}' (len {len})"
                            )
                        })?;
                        *slot = parsed_val;
                        return Ok(());
                    }
                    anyhow::bail!(
                        "cannot set field '{segment}' directly on a list in option '{spec}'"
                    );
                }
                _ => {
                    anyhow::bail!(
                        "cannot traverse non-mapping at '{}' in option '{spec}'",
                        segments[..=i].join(".")
                    );
                }
            }
        }

        match cursor {
            Value::Mapping(map) => {
                let k = Value::String(segment.clone());
                if !map.contains_key(&k) || map.get(&k).is_some_and(Value::is_null) {
                    map.insert(k.clone(), Value::Mapping(serde_yaml::Mapping::new()));
                }
                cursor = map.get_mut(&k).expect("inserted above");
            }
            Value::Sequence(seq) => {
                let pos = if let Ok(index) = segment.parse::<usize>() {
                    (index < seq.len()).then_some(index)
                } else {
                    seq.iter().position(|item| {
                        item.as_mapping().is_some_and(|m| {
                            m.get(Value::String("name".to_string()))
                                .and_then(Value::as_str)
                                == Some(segment.as_str())
                                || m.get(Value::String("keyword".to_string()))
                                    .and_then(Value::as_str)
                                    == Some(segment.as_str())
                        })
                    })
                };
                let index = pos.ok_or_else(|| {
                    anyhow::anyhow!(
                        "no entry matching '{segment}' at '{}' in option '{spec}'",
                        segments[..i].join(".")
                    )
                })?;
                cursor = &mut seq[index];
            }
            _ => {
                anyhow::bail!(
                    "cannot traverse non-mapping at '{}' in option '{spec}'",
                    segments[..=i].join(".")
                );
            }
        }
    }
    Ok(())
}

/// Validate a YAML value recursively against `schema` when the `jsonschema`
/// crate (`config-validation` feature) is disabled.
#[cfg(not(feature = "config-validation"))]
fn validate_yaml_against_schema(
    node: &serde_json::Value,
    root: &serde_json::Value,
    val: &Value,
    path: &str,
) -> Result<()> {
    fn deref<'a>(
        node: &'a serde_json::Value,
        root: &'a serde_json::Value,
    ) -> &'a serde_json::Value {
        if let Some(r) = node.get("$ref").and_then(serde_json::Value::as_str) {
            if let Some(name) = r.strip_prefix("#/$defs/") {
                if let Some(def) = root.get("$defs").and_then(|d| d.get(name)) {
                    return def;
                }
            }
        }
        node
    }

    let node = deref(node, root);
    if let Some(any_of) = node.get("anyOf").and_then(serde_json::Value::as_array) {
        let mut last_err = None;
        for branch in any_of {
            match validate_yaml_against_schema(branch, root, val, path) {
                Ok(()) => return Ok(()),
                Err(e) => last_err = Some(e),
            }
        }
        if let Some(err) = last_err {
            return Err(err);
        }
    }
    if let Some(one_of) = node.get("oneOf").and_then(serde_json::Value::as_array) {
        let mut last_err = None;
        for branch in one_of {
            match validate_yaml_against_schema(branch, root, val, path) {
                Ok(()) => return Ok(()),
                Err(e) => last_err = Some(e),
            }
        }
        if let Some(err) = last_err {
            return Err(err);
        }
    }

    let matches_type = |expected: &str| match expected {
        "null" => val.is_null(),
        "boolean" => val.is_bool(),
        "integer" => val.as_i64().is_some() || val.as_u64().is_some(),
        "number" => val.is_number(),
        "string" => val.is_string(),
        "array" => val.is_sequence(),
        "object" => val.is_mapping(),
        _ => true,
    };
    if let Some(ty) = node.get("type") {
        let ok = if let Some(s) = ty.as_str() {
            matches_type(s)
        } else if let Some(arr) = ty.as_array() {
            arr.iter()
                .filter_map(serde_json::Value::as_str)
                .any(matches_type)
        } else {
            true
        };
        if !ok {
            anyhow::bail!("invalid CLI configuration override at '{path}': unexpected value type");
        }
    }

    if let Some(enum_vals) = node.get("enum").and_then(serde_json::Value::as_array) {
        let json_val = serde_json::to_value(val).unwrap_or(serde_json::Value::Null);
        if !enum_vals.contains(&json_val) {
            anyhow::bail!("invalid CLI configuration override at '{path}': value not in enum");
        }
    }

    if let Some(n) = val.as_f64() {
        if let Some(min) = node.get("minimum").and_then(serde_json::Value::as_f64) {
            if n < min {
                anyhow::bail!(
                    "invalid CLI configuration override at '{path}': {n} is less than minimum {min}"
                );
            }
        }
        if let Some(max) = node.get("maximum").and_then(serde_json::Value::as_f64) {
            if n > max {
                anyhow::bail!(
                    "invalid CLI configuration override at '{path}': {n} exceeds maximum {max}"
                );
            }
        }
    }

    if let Some(seq) = val.as_sequence() {
        if let Some(items_schema) = node.get("items") {
            for (idx, item) in seq.iter().enumerate() {
                let child_path = format!("{path}/{idx}");
                validate_yaml_against_schema(items_schema, root, item, &child_path)?;
            }
        }
    }

    if let Some(map) = val.as_mapping() {
        let props = node
            .get("properties")
            .and_then(serde_json::Value::as_object);
        let add = node.get("additionalProperties");
        if let Some(req) = node.get("required").and_then(serde_json::Value::as_array) {
            for r in req.iter().filter_map(serde_json::Value::as_str) {
                if !map.contains_key(Value::String(r.to_string())) {
                    anyhow::bail!(
                        "invalid CLI configuration override at '{path}': missing required key '{r}'"
                    );
                }
            }
        }
        for (k, v) in map {
            let Some(k_str) = k.as_str() else {
                continue;
            };
            let child_path = format!("{path}/{k_str}");
            if let Some(prop_schema) = props.and_then(|p| p.get(k_str)) {
                validate_yaml_against_schema(prop_schema, root, v, &child_path)?;
            } else if let Some(add_schema) = add {
                if add_schema.is_object() {
                    validate_yaml_against_schema(add_schema, root, v, &child_path)?;
                } else if add_schema.as_bool() == Some(false) {
                    anyhow::bail!(
                        "invalid CLI configuration override at '{child_path}': unknown key '{k_str}'"
                    );
                }
            } else if props.is_some() {
                anyhow::bail!(
                    "invalid CLI configuration override at '{child_path}': unknown key '{k_str}'"
                );
            }
        }
    }
    Ok(())
}

/// Apply `--keep-open` and `-o` / `--option` overrides to a v1-shaped `raw`
/// config value, then validate the result against the config schema.
fn apply_cli_overrides(raw: &mut Value, args: &Args) -> Result<()> {
    if !args.keep_open && args.options.is_empty() {
        return Ok(());
    }
    if args.keep_open {
        set_yaml_override(raw, "general.close_on_action=false")?;
    }
    for spec in &args.options {
        set_yaml_override(raw, spec)?;
    }
    let problems = validate_config_value(raw);
    if let Some(first) = problems.first() {
        anyhow::bail!("invalid CLI configuration override: {first}");
    }
    #[cfg(not(feature = "config-validation"))]
    {
        let schema = serde_json::to_value(schemars::schema_for!(ConfigSchema))
            .context("cannot serialize the built-in schema")?;
        validate_yaml_against_schema(&schema, &schema, raw, "")?;
        serde_yaml::from_value::<Config>(raw.clone())
            .context("invalid CLI configuration override")?;
    }
    Ok(())
}

/// Read the configuration file and return a ParsedConfig.
pub fn read_config(filename: &str, args: &Args) -> Result<ParsedConfig> {
    let contents =
        fs::read_to_string(filename).context(format!("cannot open config file {filename}"))?;
    let mut raw: Value = serde_yaml::from_str(&contents).context("cannot parse config as YAML")?;

    if migrate_config_v0_to_v1(filename, &mut raw)? {
        // The file on disk now holds the re-serialized v1 config.
        let migrated = serde_yaml::to_string(&raw).unwrap_or_default();
        report_config_warnings(&raw, &migrated, false, filename);
    } else {
        report_config_warnings(&raw, &contents, false, filename);
    }

    let launchers_path = launchers_file_for(filename);
    if launchers_path.exists() {
        let has_inline_launchers = raw
            .as_mapping()
            .and_then(|m| m.get(Value::String("launchers".to_string())))
            .and_then(Value::as_mapping)
            .is_some_and(|m| !m.is_empty());
        if !has_inline_launchers {
            let launchers = read_launchers_file(&launchers_path)?;
            if let Some(m) = raw.as_mapping_mut() {
                m.insert(
                    Value::String("launchers".to_string()),
                    Value::Mapping(launchers),
                );
            }
        } else {
            eprintln!(
                "raffi: ignoring {} because {filename} already defines launchers",
                launchers_path.display()
            );
        }
    }

    apply_cli_overrides(&mut raw, args)?;
    let config: Config = serde_yaml::from_value(raw).context("cannot parse config")?;

    process_config(config, args)
}

/// Path of the standalone launchers file that sits next to `configfile`.
fn launchers_file_for(configfile: &str) -> PathBuf {
    Path::new(configfile)
        .parent()
        .unwrap_or(Path::new("."))
        .join("launchers.yaml")
}

/// Parse a launchers file, a mapping of launcher names to entries, and wrap
/// it as a v1 config so it can be checked against the main schema.
fn parse_launchers_file(path: &Path) -> Result<(String, Value)> {
    let contents = fs::read_to_string(path)
        .context(format!("cannot open launchers file {}", path.display()))?;
    let launchers: Value =
        serde_yaml::from_str(&contents).context("cannot parse launchers file as YAML")?;
    let launchers = if launchers.is_null() {
        Value::Mapping(serde_yaml::Mapping::new())
    } else {
        launchers
    };
    let mut wrapped = serde_yaml::Mapping::new();
    wrapped.insert(Value::String("version".to_string()), Value::from(1u64));
    wrapped.insert(Value::String("launchers".to_string()), launchers);
    Ok((contents, Value::Mapping(wrapped)))
}

/// Validate a wrapped launchers file, reporting paths relative to the file itself.
fn launchers_file_problems(contents: &str, wrapped: &Value) -> Vec<ConfigProblem> {
    let mut problems = validate_config_value(wrapped);
    locate_config_problems(&mut problems, contents, true);
    for problem in &mut problems {
        if let Some(rest) = problem.path.strip_prefix("/launchers") {
            problem.path = if rest.is_empty() { "/" } else { rest }.to_string();
        }
    }
    problems
}

fn read_launchers_file(path: &Path) -> Result<serde_yaml::Mapping> {
    let (contents, wrapped) = parse_launchers_file(path)?;
    let problems = launchers_file_problems(&contents, &wrapped);
    print_config_warnings(&problems, &path.display().to_string());
    let config: Config = serde_yaml::from_value(wrapped)
        .context(format!("cannot parse launchers file {}", path.display()))?;
    Ok(config.launchers)
}

/// Check a launchers file without running raffi.
pub fn check_launchers_file(path: &Path) -> Result<Vec<ConfigProblem>> {
    ensure_config_validation()?;
    let (contents, wrapped) = parse_launchers_file(path)?;
    let mut problems = launchers_file_problems(&contents, &wrapped);
    if problems.is_empty() {
        if let Err(e) = serde_yaml::from_value::<Config>(wrapped) {
            problems.push(ConfigProblem::new("/", e.to_string()));
        }
    }
    Ok(problems)
}

/// Read config from a reader, accepting both v0 (flat) and v1 (launchers) formats.
/// Used primarily for tests where no file path is available for migration.
pub fn read_config_from_reader<R: Read>(reader: R, args: &Args) -> Result<ParsedConfig> {
    let mut contents = String::new();
    let mut reader = reader;
    reader
        .read_to_string(&mut contents)
        .context("cannot read config")?;
    let mut raw: Value = serde_yaml::from_str(&contents).context("cannot parse config")?;

    // For reader-based configs, do an in-memory migration (no file write)
    let v0 = migrate_config_v0_in_memory(&mut raw);
    report_config_warnings(&raw, &contents, v0, "config");
    apply_cli_overrides(&mut raw, args)?;

    let config: Config = serde_yaml::from_value(raw).context("cannot parse config")?;
    process_config(config, args)
}

/// Common config processing logic shared by read_config and read_config_from_reader.
fn process_config(config: Config, args: &Args) -> Result<ParsedConfig> {
    let mut rafficonfigs = Vec::new();

    for value in config.launchers.values() {
        if value.is_mapping() {
            let mut mc: RaffiConfig = serde_yaml::from_value(value.clone())
                .context("cannot parse config entry".to_string())?;
            mc.binary = mc.binary.map(|s| expand_config_value(&s));
            mc.icon = mc.icon.map(|s| expand_config_value(&s));
            mc.ifexist = mc.ifexist.map(|s| expand_config_value(&s));
            mc.args = mc
                .args
                .map(|v| v.into_iter().map(|s| expand_config_value(&s)).collect());
            if mc.disabled.unwrap_or(false)
                || !is_valid_config(&mut mc, args, &DefaultEnvProvider, &DefaultBinaryChecker)
            {
                continue;
            }
            rafficonfigs.push(mc);
        }
    }

    let mut addons = config.addons;
    let fb = &mut addons.file_browser;
    fb.action = fb.action.as_ref().map(|s| expand_config_value(s));
    fb.secondary_action = fb.secondary_action.as_ref().map(|s| expand_config_value(s));
    fb.file_manager = fb.file_manager.as_ref().map(|s| expand_config_value(s));
    for sf in &mut addons.script_filters {
        sf.command = expand_config_value(&sf.command);
        sf.icon = sf.icon.as_ref().map(|s| expand_config_value(s));
        sf.action = sf.action.as_ref().map(|s| expand_config_value(s));
        sf.secondary_action = sf.secondary_action.as_ref().map(|s| expand_config_value(s));
    }
    let bm = &mut addons.bookmarks;
    for source in bm.sources.iter_mut().flatten() {
        source.path = source.path.as_ref().map(|s| expand_config_value(s));
        if let Some(BookmarkOpener::Command(argv)) = &mut source.open_with {
            argv.iter_mut()
                .for_each(|arg| *arg = expand_config_value(arg));
        }
    }
    if let Some(BookmarkOpener::Command(argv)) = &mut bm.open_with {
        argv.iter_mut()
            .for_each(|arg| *arg = expand_config_value(arg));
    }
    let bh = &mut addons.browser_history;
    for source in bh.sources.iter_mut().flatten() {
        source.path = source.path.as_ref().map(|s| expand_config_value(s));
        if let Some(BookmarkOpener::Command(argv)) = &mut source.open_with {
            argv.iter_mut()
                .for_each(|arg| *arg = expand_config_value(arg));
        }
    }
    if let Some(BookmarkOpener::Command(argv)) = &mut bh.open_with {
        argv.iter_mut()
            .for_each(|arg| *arg = expand_config_value(arg));
    }
    for ws in &mut addons.web_searches {
        ws.url = expand_config_value(&ws.url);
        ws.icon = ws.icon.as_ref().map(|s| expand_config_value(s));
    }
    for ts in &mut addons.text_snippets {
        ts.icon = ts.icon.as_ref().map(|s| expand_config_value(s));
        ts.file = ts.file.as_ref().map(|s| expand_config_value(s));
        ts.command = ts.command.as_ref().map(|s| expand_config_value(s));
        ts.directory = ts.directory.as_ref().map(|s| expand_config_value(s));
    }
    addons.launcher_keywords = launcher_keywords(&rafficonfigs);

    Ok(ParsedConfig {
        general: config.general,
        addons,
        entries: rafficonfigs,
    })
}

/// Validate the RaffiConfig based on various conditions.
fn is_valid_config(
    mc: &mut RaffiConfig,
    args: &Args,
    env_provider: &impl EnvProvider,
    binary_checker: &impl BinaryChecker,
) -> bool {
    if let Some(alias) = &mc.alias {
        let name = mc.description.as_deref().unwrap_or(alias);
        if alias.trim().is_empty() {
            eprintln!("raffi: config warning: launcher '{name}' has an empty alias, skipped");
            return false;
        }
        if mc.binary.is_some() || mc.script.is_some() || mc.args.is_some() {
            eprintln!(
                "raffi: config warning: launcher '{name}' combines alias with binary, script or args, skipped"
            );
            return false;
        }
        if let Some(keyword) = &mc.keyword {
            if keyword.is_empty() || keyword.chars().any(char::is_whitespace) {
                eprintln!(
                    "raffi: config warning: launcher '{name}' has a keyword that is empty or contains spaces, skipped"
                );
                return false;
            }
        }
        if mc.description.is_none() {
            mc.description = Some(alias.clone());
        }
    } else if mc.keyword.is_some() {
        let name = mc
            .description
            .as_deref()
            .or(mc.binary.as_deref())
            .unwrap_or("?");
        eprintln!("raffi: config warning: launcher '{name}' has a keyword but no alias, skipped");
        return false;
    } else if let Some(_script) = &mc.script {
        if !binary_checker.exists(
            mc.binary
                .as_deref()
                .unwrap_or(args.default_script_shell.as_deref().unwrap_or("bash")),
        ) {
            return false;
        }
    } else if let Some(binary) = &mc.binary {
        if !binary_checker.exists(binary) {
            return false;
        }
    } else if let Some(description) = &mc.description {
        mc.binary = Some(description.clone());
    } else {
        return false;
    }

    mc.ifenveq
        .as_ref()
        .is_none_or(|eq| eq.len() == 2 && env_provider.var(&eq[0]).unwrap_or_default() == eq[1])
        && mc
            .ifenvset
            .as_ref()
            .is_none_or(|var| env_provider.var(var).is_ok())
        && mc
            .ifenvnotset
            .as_ref()
            .is_none_or(|var| env_provider.var(var).is_err())
        && mc
            .ifexist
            .as_ref()
            .is_none_or(|exist| binary_checker.exists(exist))
}

/// The launcher the UI picked, by description or binary. Alias launchers are
/// routed inside the UI and are never executed, so they are skipped.
fn find_chosen_entry<'a>(entries: &'a [RaffiConfig], chosen: &str) -> Option<&'a RaffiConfig> {
    entries.iter().find(|mc| {
        mc.alias.is_none()
            && (mc.description.as_deref() == Some(chosen) || mc.binary.as_deref() == Some(chosen))
    })
}

/// Check if a binary exists in the PATH.
pub(crate) fn find_binary(binary: &str) -> bool {
    if binary.contains('/') {
        return Path::new(binary).exists();
    }

    std::env::var("PATH")
        .unwrap_or_default()
        .split(':')
        .any(|path| Path::new(&format!("{path}/{binary}")).exists())
}

#[cfg(feature = "wayland")]
fn default_ui_type(_fuzzel_available: bool, _fallbacks_configured: bool) -> Result<UIType> {
    Ok(UIType::Native)
}

#[cfg(not(feature = "wayland"))]
fn default_ui_type(fuzzel_available: bool, fallbacks_configured: bool) -> Result<UIType> {
    if fuzzel_available && !fallbacks_configured {
        Ok(UIType::Fuzzel)
    } else {
        Err(anyhow::anyhow!(
            "No UI backend available. Install 'fuzzel' or build with the 'wayland' feature."
        ))
    }
}

fn xdg_data_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();

    if let Ok(data_home) = std::env::var("XDG_DATA_HOME") {
        dirs.push(PathBuf::from(data_home));
    } else if let Ok(home) = std::env::var("HOME") {
        dirs.push(PathBuf::from(home).join(".local/share"));
    }

    let system_dirs = std::env::var("XDG_DATA_DIRS")
        .unwrap_or_else(|_| "/usr/local/share:/usr/share".to_string());
    dirs.extend(std::env::split_paths(&system_dirs));

    dirs
}

fn desktop_entry_dirs() -> Vec<PathBuf> {
    xdg_data_dirs()
        .into_iter()
        .map(|dir| dir.join("applications"))
        .collect()
}

fn parse_bool(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "true" | "1" | "yes"
    )
}

fn parse_desktop_entry_keys(contents: &str) -> HashMap<String, String> {
    let mut in_desktop_entry = false;
    let mut keys = HashMap::new();

    for line in contents.lines() {
        let trimmed = line.trim();

        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            if trimmed == "[Desktop Entry]" {
                in_desktop_entry = true;
                continue;
            }

            if in_desktop_entry {
                break;
            }

            continue;
        }

        if !in_desktop_entry {
            continue;
        }

        if let Some((key, value)) = trimmed.split_once('=') {
            keys.insert(key.trim().to_string(), value.trim().to_string());
        }
    }

    keys
}

fn split_desktop_exec(exec: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut chars = exec.chars().peekable();
    let mut in_single = false;
    let mut in_double = false;

    while let Some(ch) = chars.next() {
        match ch {
            '\\' => {
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            c if c.is_whitespace() && !in_single && !in_double => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(ch),
        }
    }

    if !current.is_empty() {
        tokens.push(current);
    }

    tokens
}

fn strip_desktop_exec_field_codes(token: &str) -> String {
    let mut cleaned = String::with_capacity(token.len());
    let mut chars = token.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '%' {
            match chars.peek().copied() {
                Some('%') => {
                    cleaned.push('%');
                    chars.next();
                }
                Some(code) if "fFuUdDnNickvm".contains(code) => {
                    chars.next();
                }
                _ => cleaned.push(ch),
            }
        } else {
            cleaned.push(ch);
        }
    }

    cleaned.trim().to_string()
}

fn parse_desktop_exec(exec: &str) -> Option<(String, Vec<String>)> {
    let mut tokens = split_desktop_exec(exec)
        .into_iter()
        .map(|token| strip_desktop_exec_field_codes(&token))
        .filter(|token| !token.is_empty())
        .collect::<Vec<_>>();

    if tokens.is_empty() {
        return None;
    }

    let binary = tokens.remove(0);
    Some((binary, tokens))
}

/// What a scan of the installed desktop applications lists.
pub(crate) struct DesktopScan {
    /// List the actions applications declare, as their own entries.
    pub actions: bool,
    /// Terminal command line prefix for `Terminal=true` applications, left
    /// out when there is none.
    pub terminal: Option<Vec<String>>,
    /// Desktop names from `XDG_CURRENT_DESKTOP`, for `OnlyShowIn` and
    /// `NotShowIn`.
    pub desktops: Vec<String>,
}

impl DesktopScan {
    fn from_addons(addons: &AddonsConfig) -> Self {
        let terminal = &addons.terminal;
        Self {
            actions: addons.apps.actions,
            terminal: terminal
                .enabled
                .then(|| terminal::resolve_default(terminal.command.as_deref()))
                .flatten(),
            desktops: std::env::var("XDG_CURRENT_DESKTOP")
                .unwrap_or_default()
                .split(':')
                .filter(|desktop| !desktop.is_empty())
                .map(str::to_string)
                .collect(),
        }
    }

    /// Whether an entry with these `OnlyShowIn` and `NotShowIn` values shows
    /// on the current desktop.
    fn shows_on_desktop(&self, keys: &HashMap<String, String>) -> bool {
        let listed = |key: &str| {
            keys.get(key).map(|value| {
                value.split(';').any(|name| {
                    self.desktops
                        .iter()
                        .any(|desktop| !name.is_empty() && desktop.eq_ignore_ascii_case(name))
                })
            })
        };
        listed("OnlyShowIn") != Some(false) && listed("NotShowIn") != Some(true)
    }

    /// The command line running `exec`, through the terminal when
    /// `in_terminal`; None when it cannot run.
    fn command(
        &self,
        exec: &str,
        in_terminal: bool,
        binary_checker: &impl BinaryChecker,
    ) -> Option<(String, Vec<String>)> {
        let (binary, args) = parse_desktop_exec(exec)?;
        if !binary_checker.exists(&binary) {
            return None;
        }
        if !in_terminal {
            return Some((binary, args));
        }
        let mut argv = terminal::exec_argv(self.terminal.as_deref()?, &binary, &args);
        let program = argv.remove(0);
        Some((program, argv))
    }
}

/// The groups of a desktop entry file, in order, with their keys.
fn parse_desktop_groups(contents: &str) -> Vec<(String, HashMap<String, String>)> {
    let mut groups: Vec<(String, HashMap<String, String>)> = Vec::new();
    for line in contents.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some(name) = trimmed
            .strip_prefix('[')
            .and_then(|rest| rest.strip_suffix(']'))
        {
            groups.push((name.to_string(), HashMap::new()));
            continue;
        }
        if let (Some((_, keys)), Some((key, value))) = (groups.last_mut(), trimmed.split_once('='))
        {
            keys.entry(key.trim().to_string())
                .or_insert_with(|| value.trim().to_string());
        }
    }
    groups
}

/// The launcher entries of a desktop entry file: the application, then its
/// actions when the scan lists them.
fn parse_desktop_entry_file(
    path: &Path,
    scan: &DesktopScan,
    binary_checker: &impl BinaryChecker,
) -> Vec<RaffiConfig> {
    let Ok(contents) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let groups = parse_desktop_groups(&contents);
    let group = |name: &str| {
        groups
            .iter()
            .find(|(group, _)| group == name)
            .map(|(_, keys)| keys)
    };
    let Some(keys) = group("Desktop Entry") else {
        return Vec::new();
    };

    if keys.get("Type").map(String::as_str) != Some("Application")
        || keys.get("Hidden").is_some_and(|value| parse_bool(value))
        || keys.get("NoDisplay").is_some_and(|value| parse_bool(value))
        || !scan.shows_on_desktop(keys)
    {
        return Vec::new();
    }
    let Some(name) = keys.get("Name") else {
        return Vec::new();
    };
    if keys
        .get("TryExec")
        .is_some_and(|try_exec| !binary_checker.exists(try_exec))
    {
        return Vec::new();
    }
    let in_terminal = keys.get("Terminal").is_some_and(|value| parse_bool(value));
    let icon = keys.get("Icon").cloned();
    let entry = |description: String, exec: &str, icon: Option<String>| {
        let (binary, args) = scan.command(exec, in_terminal, binary_checker)?;
        Some(RaffiConfig {
            binary: Some(binary),
            args: (!args.is_empty()).then_some(args),
            icon,
            description: Some(description),
            ..Default::default()
        })
    };

    let mut entries = Vec::new();
    entries.extend(
        keys.get("Exec")
            .and_then(|exec| entry(name.clone(), exec, icon.clone())),
    );
    if scan.actions {
        let actions = keys.get("Actions").map(String::as_str).unwrap_or_default();
        for id in actions.split(';').filter(|id| !id.is_empty()) {
            let Some(action) = group(&format!("Desktop Action {id}")) else {
                continue;
            };
            let (Some(action_name), Some(exec)) = (action.get("Name"), action.get("Exec")) else {
                continue;
            };
            entries.extend(entry(
                format!("{name}: {action_name}"),
                exec,
                action.get("Icon").cloned().or_else(|| icon.clone()),
            ));
        }
    }
    entries
}

fn detect_desktop_entries_from_dirs(
    dirs: &[PathBuf],
    scan: &DesktopScan,
    binary_checker: &impl BinaryChecker,
) -> Vec<RaffiConfig> {
    let mut seen = HashSet::new();
    let mut ids = HashSet::new();
    let mut entries = Vec::new();

    for dir in dirs {
        if !dir.exists() {
            continue;
        }

        for entry in walkdir::WalkDir::new(dir)
            .follow_links(true)
            .sort_by_file_name()
            .into_iter()
            .filter_map(Result::ok)
        {
            if !entry.file_type().is_file() {
                continue;
            }
            if entry.path().extension().and_then(|ext| ext.to_str()) != Some("desktop") {
                continue;
            }
            // The first file with a desktop file ID wins, even when it
            // hides the application: that is how a user file overrides
            // a system one.
            let id = entry
                .path()
                .strip_prefix(dir)
                .unwrap_or(entry.path())
                .to_string_lossy()
                .replace('/', "-");
            if !ids.insert(id) {
                continue;
            }

            for config in parse_desktop_entry_file(entry.path(), scan, binary_checker) {
                let key = format!(
                    "{}\u{1f}{}\u{1f}{}",
                    config.description.as_deref().unwrap_or_default(),
                    config.binary.as_deref().unwrap_or_default(),
                    config.args.as_deref().unwrap_or(&[]).join("\u{1f}")
                );
                if seen.insert(key) {
                    entries.push(config);
                }
            }
        }
    }

    entries.sort_by(|left, right| {
        left.description
            .as_deref()
            .unwrap_or_default()
            .cmp(right.description.as_deref().unwrap_or_default())
    });
    entries
}

fn detect_desktop_entries(
    scan: &DesktopScan,
    binary_checker: &impl BinaryChecker,
) -> Vec<RaffiConfig> {
    detect_desktop_entries_from_dirs(&desktop_entry_dirs(), scan, binary_checker)
}

/// Add the installed applications after the launchers, leaving out those
/// named like a launcher or running the same command.
fn merge_apps(entries: &mut Vec<RaffiConfig>, apps: Vec<RaffiConfig>) {
    let launchers = entries.len();
    for app in apps {
        let duplicate = entries[..launchers].iter().any(|launcher| {
            let same_name = matches!(
                (&launcher.description, &app.description),
                (Some(left), Some(right)) if left.eq_ignore_ascii_case(right)
            );
            let same_command = launcher.binary.is_some()
                && launcher.binary == app.binary
                && launcher.args.as_deref().unwrap_or_default()
                    == app.args.as_deref().unwrap_or_default();
            same_name || same_command
        });
        if !duplicate {
            entries.push(app);
        }
    }
}

const DEFAULT_ICON_PATH: &str = "assets/default_icon.svg";
const DEFAULT_ICON_BYTES: &[u8] = include_bytes!("../assets/default_icon.svg");
const GENERIC_ICON_NAMES: &[&str] = &[
    "application-x-executable",
    "application-default-icon",
    "application-x-desktop",
];

pub(crate) fn icon_cache_dir() -> PathBuf {
    PathBuf::from(
        std::env::var("XDG_CACHE_HOME")
            .unwrap_or_else(|_| format!("{}/.cache", std::env::var("HOME").unwrap_or_default())),
    )
    .join("raffi")
}

fn is_supported_icon_extension(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|ext| ext.to_str()),
        Some("png" | "svg")
    )
}

fn is_icon_file(path: &Path) -> bool {
    path.is_file() && is_supported_icon_extension(path)
}

fn push_icon_candidate(candidates: &mut Vec<String>, seen: &mut HashSet<String>, candidate: &str) {
    let trimmed = candidate.trim();
    if trimmed.is_empty() {
        return;
    }

    let owned = trimmed.to_string();
    if seen.insert(owned.clone()) {
        candidates.push(owned);
    }

    let Some(file_name) = Path::new(trimmed)
        .file_name()
        .and_then(|name| name.to_str())
    else {
        return;
    };
    if file_name == trimmed {
        return;
    }

    let basename = file_name.to_string();
    if seen.insert(basename.clone()) {
        candidates.push(basename);
    }
}

fn launcher_icon_candidates(config: &RaffiConfig) -> Vec<String> {
    let mut candidates = Vec::new();
    let mut seen = HashSet::new();

    if let Some(icon) = config.icon.as_deref() {
        push_icon_candidate(&mut candidates, &mut seen, icon);
    }
    if let Some(binary) = config.binary.as_deref() {
        push_icon_candidate(&mut candidates, &mut seen, binary);
    }
    if let Some(args) = config.args.as_deref() {
        for arg in args {
            if arg.starts_with('-') || arg.contains("://") {
                continue;
            }
            push_icon_candidate(&mut candidates, &mut seen, arg);
        }
    }

    candidates
}

fn is_exec_wrapper_binary(candidate: &str) -> bool {
    matches!(
        Path::new(candidate)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(candidate),
        "env" | "sh" | "bash" | "dash" | "zsh"
    )
}

fn desktop_exec_icon_candidates(exec: &str, binary_checker: &impl BinaryChecker) -> Vec<String> {
    let mut candidates = Vec::new();
    let mut seen = HashSet::new();

    for token in split_desktop_exec(exec)
        .into_iter()
        .map(|token| strip_desktop_exec_field_codes(&token))
        .filter(|token| !token.is_empty())
    {
        if token.starts_with('-') || token.contains('=') || is_exec_wrapper_binary(&token) {
            continue;
        }
        if !binary_checker.exists(&token) {
            continue;
        }
        push_icon_candidate(&mut candidates, &mut seen, &token);
    }

    candidates
}

fn desktop_entry_icon_map_from_dirs(
    dirs: &[PathBuf],
    binary_checker: &impl BinaryChecker,
) -> HashMap<String, String> {
    let mut icon_map = HashMap::new();

    for dir in dirs {
        if !dir.exists() {
            continue;
        }

        for entry in walkdir::WalkDir::new(dir)
            .max_depth(2)
            .follow_links(true)
            .into_iter()
            .filter_map(Result::ok)
        {
            if !entry.file_type().is_file() {
                continue;
            }
            if entry.path().extension().and_then(|ext| ext.to_str()) != Some("desktop") {
                continue;
            }

            let Ok(contents) = fs::read_to_string(entry.path()) else {
                continue;
            };
            let keys = parse_desktop_entry_keys(&contents);

            if keys.get("Type").map(String::as_str) != Some("Application") {
                continue;
            }
            if keys.get("Hidden").is_some_and(|value| parse_bool(value))
                || keys.get("NoDisplay").is_some_and(|value| parse_bool(value))
                || keys.get("Terminal").is_some_and(|value| parse_bool(value))
            {
                continue;
            }

            let Some(icon) = keys.get("Icon").cloned() else {
                continue;
            };

            let mut candidates = keys
                .get("Exec")
                .map(|exec| desktop_exec_icon_candidates(exec, binary_checker))
                .unwrap_or_default();

            if let Some(try_exec) = keys.get("TryExec") {
                if binary_checker.exists(try_exec) && !is_exec_wrapper_binary(try_exec) {
                    let mut try_exec_seen = candidates.iter().cloned().collect::<HashSet<_>>();
                    push_icon_candidate(&mut candidates, &mut try_exec_seen, try_exec);
                }
            }

            for candidate in candidates {
                icon_map.entry(candidate).or_insert_with(|| icon.clone());
            }
        }
    }

    icon_map
}

pub(crate) fn read_desktop_icon_map() -> HashMap<String, String> {
    desktop_entry_icon_map_from_dirs(&desktop_entry_dirs(), &DefaultBinaryChecker)
}

pub(crate) fn resolve_icon_reference(
    icon: &str,
    icon_map: &HashMap<String, String>,
) -> Option<String> {
    let expanded = expand_config_value(icon);
    if is_icon_file(Path::new(&expanded)) {
        return Some(expanded);
    }

    icon_map.get(icon).cloned().or_else(|| {
        Path::new(icon)
            .file_stem()
            .and_then(|stem| stem.to_str())
            .and_then(|stem| icon_map.get(stem).cloned())
    })
}

fn bundled_default_icon_path() -> Option<String> {
    let cache_dir = icon_cache_dir();
    fs::create_dir_all(&cache_dir).ok()?;

    let path = cache_dir.join(Path::new(DEFAULT_ICON_PATH).file_name().unwrap_or_default());
    let needs_write = fs::read(&path)
        .map(|contents| contents != DEFAULT_ICON_BYTES)
        .unwrap_or(true);
    if needs_write {
        fs::write(&path, DEFAULT_ICON_BYTES).ok()?;
    }

    Some(path.to_string_lossy().into_owned())
}

pub(crate) fn default_icon_path(icon_map: &HashMap<String, String>) -> Option<String> {
    GENERIC_ICON_NAMES
        .iter()
        .find_map(|icon| icon_map.get(*icon).cloned())
        .or_else(bundled_default_icon_path)
}

pub(crate) fn resolve_launcher_icon_path(
    config: &RaffiConfig,
    icon_map: &HashMap<String, String>,
    desktop_icon_map: &HashMap<String, String>,
) -> Option<String> {
    let candidates = launcher_icon_candidates(config);

    for candidate in &candidates {
        if let Some(path) = resolve_icon_reference(candidate, icon_map) {
            return Some(path);
        }
    }

    for candidate in &candidates {
        if let Some(icon) = desktop_icon_map.get(candidate) {
            if let Some(path) = resolve_icon_reference(icon, icon_map) {
                return Some(path);
            }
        }
    }

    default_icon_path(icon_map)
}

fn no_valid_config_message(configfile: &str) -> String {
    let launchers = launchers_file_for(configfile);
    format!(
        "No valid launchers found.\nChecked: {configfile} and {}\nDefine launchers in launchers.yaml next to raffi.yaml, or in a `launchers:` section of raffi.yaml.\nYou can copy `examples/launchers.yaml` as a starting point, or install desktop applications so Raffi can auto-detect them.",
        launchers.display()
    )
}

/// Save the icon map to a cache file.
fn save_to_cache_file(map: &HashMap<String, String>) -> Result<()> {
    let cache_dir = icon_cache_dir();
    fs::create_dir_all(&cache_dir).context("Failed to create cache directory")?;

    let cache_file_path = cache_dir.join("icon.cache");
    let mut cache_file = File::create(&cache_file_path).context("Failed to create cache file")?;
    cache_file
        .write_all(
            serde_json::to_string(map)
                .context("Failed to serialize icon map")?
                .as_bytes(),
        )
        .context("Failed to write to cache file")?;
    Ok(())
}

/// Clear the icon cache file to force regeneration.
pub fn clear_icon_cache() -> Result<()> {
    let cache_path = icon_cache_dir().join("icon.cache");
    if Path::new(&cache_path).exists() {
        fs::remove_file(&cache_path).context("Failed to remove icon cache file")?;
    }
    #[cfg(feature = "wayland")]
    {
        let thumbnails = icon_cache_dir().join(ui::THUMBNAIL_DIR_NAME);
        if thumbnails.exists() {
            fs::remove_dir_all(&thumbnails).context("Failed to remove icon thumbnails")?;
        }
    }
    Ok(())
}

/// Clear the cached emoji data files to force re-download.
pub fn clear_emoji_cache() -> Result<()> {
    let emoji_dir = icon_cache_dir().join("emoji");
    if Path::new(&emoji_dir).exists() {
        fs::remove_dir_all(&emoji_dir).context("Failed to remove emoji cache directory")?;
    }
    Ok(())
}

/// Read the icon map from the cache file or generate it if it doesn't exist.
pub fn read_icon_map() -> Result<HashMap<String, String>> {
    let cache_path = icon_cache_dir().join("icon.cache");

    if !Path::new(&cache_path).exists() {
        let icon_map = get_icon_map()?;
        save_to_cache_file(&icon_map)?;
        return Ok(icon_map);
    }

    let mut cache_file = File::open(&cache_path).context("Failed to open cache file")?;
    let mut contents = String::new();
    cache_file
        .read_to_string(&mut contents)
        .context("Failed to read cache file")?;
    serde_json::from_str(&contents).context("Failed to deserialize cache file")
}

/// Quote a string for safe use as a single POSIX shell word.
/// Already-safe tokens are left as-is so the output stays readable.
fn shell_quote(value: &str) -> String {
    let is_safe = !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_./:=@%+-".contains(&b));
    if is_safe {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// Render a word that appears in command position.
///
/// The exec path uses `Command::new`, which always runs an external program, while a shell would
/// first resolve the name as an alias, function, reserved word or builtin. Bare names are therefore
/// routed through `env`, which performs the same `PATH` lookup, so that a launcher named `echo` or
/// `source` behaves the same whether Raffi runs it or a shell does.
fn format_command_word(value: &str) -> String {
    if value.contains('/') {
        // A path is never resolved as an alias, function, reserved word or builtin.
        return shell_quote(value);
    }
    if value.contains('=') {
        // `env` would read this as an environment assignment. Quoting is enough on its own here,
        // because no alias, function name, reserved word or builtin can contain '='.
        return format!("'{}'", value.replace('\'', r"'\''"));
    }
    format!("env -- {}", shell_quote(value))
}

/// Format the command that would be run as a shell-ready, quoted command line.
///
/// The output is meant to be evaluated by a shell (`sh -c "$cmd"`, `swaymsg exec -- "$cmd"`,
/// `hyprctl dispatch exec "$cmd"`), not to be word-split by `xargs`. Script entries are rendered as
/// `<interpreter> -c '<script>' [<interpreter> <args>...]`, mirroring how [`execute_chosen_command`]
/// spawns them; a multi-line script therefore stays multi-line inside its quotes.
fn format_print_only_command(mc: &RaffiConfig, interpreter: &str) -> Result<String> {
    let mut parts = Vec::new();

    if let Some(script) = &mc.script {
        parts.push(format_command_word(interpreter));
        parts.push("-c".to_string());
        parts.push(shell_quote(script));
        if let Some(args) = &mc.args {
            // Passed as $0 by the exec path, so the script can reference $1..$n.
            parts.push(shell_quote(interpreter));
            parts.extend(args.iter().map(|arg| shell_quote(arg)));
        }
    } else {
        let binary = mc.binary.as_deref().context("Binary not found")?;
        parts.push(format_command_word(binary));
        parts.extend(
            mc.args
                .as_deref()
                .unwrap_or(&[])
                .iter()
                .map(|arg| shell_quote(arg)),
        );
    }

    Ok(parts.join(" "))
}

/// Spawn `command` and reap the child in a background thread so it never
/// lingers as a zombie when Raffi stays open.
fn spawn_and_reap(mut command: Command) -> std::io::Result<()> {
    let mut child = command.spawn()?;
    let _ = std::thread::Builder::new()
        .name("raffi-reaper".to_string())
        .spawn(move || {
            let _ = child.wait();
        });
    Ok(())
}

/// Spawn a launcher entry's binary or script in the background.
pub(crate) fn spawn_launcher_command(mc: &RaffiConfig, interpreter: &str) -> Result<()> {
    if let Some(script) = &mc.script {
        let mut command = Command::new(interpreter);
        command.arg("-c").arg(script);
        if let Some(args) = &mc.args {
            command.arg(interpreter);
            command.args(args);
        }
        spawn_and_reap(command).context("cannot launch script")?;
    } else {
        let mut command = Command::new(mc.binary.as_deref().context("Binary not found")?);
        command.args(mc.args.as_deref().unwrap_or(&[]));
        spawn_and_reap(command).context("cannot launch command")?;
    }
    Ok(())
}

/// Execute the chosen command or script.
pub fn execute_chosen_command(mc: &RaffiConfig, args: &Args, interpreter: &str) -> Result<()> {
    if args.print_only {
        println!("{}", format_print_only_command(mc, interpreter)?);
        return Ok(());
    }
    spawn_launcher_command(mc, interpreter)
}

/// Percent-encode a query string for use in URLs.
/// Encodes all characters except unreserved ones (A-Z, a-z, 0-9, '-', '.', '_', '~').
pub fn url_encode_query(query: &str) -> String {
    let mut encoded = String::with_capacity(query.len() * 3);
    for byte in query.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(byte as char);
            }
            _ => {
                write!(encoded, "%{:02X}", byte).unwrap();
            }
        }
    }
    encoded
}

/// Build a web search URL by replacing `{query}` in the template with the percent-encoded query,
/// then open it with `xdg-open`.
pub fn execute_web_search_url(url_template: &str, query: &str) -> Result<()> {
    let encoded = url_encode_query(query);
    let url = url_template.replace("{query}", &encoded);
    let mut command = Command::new("xdg-open");
    command.arg(&url);
    spawn_and_reap(command).context("cannot open web search URL")?;
    Ok(())
}

/// Generate the JSON Schema for the raffi config format.
fn generate_schema() -> String {
    let schema = schemars::schema_for!(ConfigSchema);
    serde_json::to_string_pretty(&schema).expect("Failed to serialize JSON Schema")
}

/// JSON Schema for `launchers.yaml`, a mapping of launcher names to entries.
fn generate_launchers_schema() -> String {
    let schema = schemars::schema_for!(HashMap<String, RaffiConfig>);
    serde_json::to_string_pretty(&schema).expect("Failed to serialize JSON Schema")
}

/// Write `schema` to `schema_path` when missing or out of date.
fn write_schema_if_changed(schema_path: &Path, schema: &str) -> Result<()> {
    if fs::read_to_string(schema_path).is_ok_and(|current| current == schema) {
        return Ok(());
    }
    let dir = schema_path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(dir)
        .with_context(|| format!("could not create schema directory {}", dir.display()))?;
    fs::write(schema_path, schema)
        .with_context(|| format!("could not write schema file {}", schema_path.display()))
}

fn write_schemas_for_config(configfile: &str) -> Result<()> {
    let config_dir = Path::new(configfile).parent().unwrap_or(Path::new("."));
    write_schema_if_changed(&config_dir.join("raffi-schema.json"), &generate_schema())?;
    write_schema_if_changed(
        &config_dir.join("launchers-schema.json"),
        &generate_launchers_schema(),
    )?;
    Ok(())
}

pub fn run(args: Args) -> Result<()> {
    if let Some(ref path) = args.debug_file {
        debug::init(path);
        debug_log!("raffi starting, debug log: {path}");
    }

    if args.version {
        println!("raffi version {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    if args.install_browser_host || args.uninstall_browser_host {
        return browser_host::manifest_command(args.uninstall_browser_host, args.force);
    }

    if args.refresh_cache {
        clear_icon_cache()?;
        clear_emoji_cache()?;
        #[cfg(feature = "wayland")]
        ui::clear_slack_cache();
    }

    let default_config_path = format!(
        "{}/.config/raffi/raffi.yaml",
        std::env::var("HOME").unwrap_or_default()
    );
    let configfile = args.configfile.as_deref().unwrap_or(&default_config_path);
    debug_log!("config: using file {configfile}");
    let config_exists = Path::new(configfile).exists();
    let launchers_path = launchers_file_for(configfile);

    if args.generate_schemas {
        write_schemas_for_config(configfile)?;
        return Ok(());
    }

    if args.check_config {
        let mut checks = Vec::new();
        if config_exists || !launchers_path.exists() {
            checks.push((configfile.to_string(), check_config(configfile)?));
        }
        if launchers_path.exists() {
            checks.push((
                launchers_path.display().to_string(),
                check_launchers_file(&launchers_path)?,
            ));
        }
        let mut count = 0;
        for (source, problems) in &checks {
            if problems.is_empty() {
                println!("{source}: config OK");
            }
            for problem in problems {
                eprintln!("{}", format_config_problem(source, problem));
            }
            count += problems.len();
        }
        if count > 0 {
            anyhow::bail!("{count} problem(s) found");
        }
        return Ok(());
    }

    // Keep the schema files next to the config in sync with this binary
    if let Err(e) = write_schemas_for_config(configfile) {
        eprintln!("Warning: could not write schema files next to config: {e:#}");
    }

    let mut parsed_config = if config_exists {
        read_config(configfile, &args).context("Failed to read config")?
    } else if launchers_path.exists() {
        let mut root = serde_yaml::Mapping::new();
        root.insert(Value::String("version".to_string()), Value::from(1u64));
        root.insert(
            Value::String("launchers".to_string()),
            Value::Mapping(read_launchers_file(&launchers_path)?),
        );
        let mut raw = Value::Mapping(root);
        apply_cli_overrides(&mut raw, &args)?;
        let config: Config = serde_yaml::from_value(raw).context("cannot parse config")?;
        process_config(config, &args)?
    } else {
        let mut raw = Value::Mapping(serde_yaml::Mapping::new());
        apply_cli_overrides(&mut raw, &args)?;
        let has_main_view_override = raw
            .as_mapping()
            .and_then(|m| m.get(Value::String("general".to_string())))
            .and_then(Value::as_mapping)
            .is_some_and(|g| g.contains_key(Value::String("main_view".to_string())));
        let mut config: Config = serde_yaml::from_value(raw).context("cannot parse config")?;
        if !has_main_view_override {
            config.general.main_view = first_run_general().main_view;
        }
        process_config(config, &args)?
    };
    debug_log!("config: loaded {} entries", parsed_config.entries.len());

    if parsed_config.entries.is_empty() {
        let scan = DesktopScan::from_addons(&parsed_config.addons);
        let detected_entries = detect_desktop_entries(&scan, &DefaultBinaryChecker);
        if detected_entries.is_empty() {
            eprintln!("{}", no_valid_config_message(configfile));
            std::process::exit(1);
        }

        if !parsed_config.addons.apps.enabled {
            eprintln!(
                "No valid launchers found in {configfile}; using {} auto-detected desktop applications.",
                detected_entries.len()
            );
        }
        parsed_config.entries = detected_entries;
    } else if parsed_config.addons.apps.enabled {
        let scan = DesktopScan::from_addons(&parsed_config.addons);
        let apps = detect_desktop_entries(&scan, &DefaultBinaryChecker);
        debug_log!("config: {} installed applications", apps.len());
        merge_apps(&mut parsed_config.entries, apps);
    }

    // Merge general config: CLI flags override config values
    let general = &parsed_config.general;
    let no_icons = args.no_icons || general.no_icons.unwrap_or(false);
    let ui_type_str = args.ui_type.as_ref().or(general.ui_type.as_ref());
    let default_script_shell = args
        .default_script_shell
        .as_deref()
        .or(general.default_script_shell.as_deref())
        .unwrap_or("bash")
        .to_string();

    // Determine theme
    let theme_str = args.theme.as_ref().or(general.theme.as_ref());
    let theme = if let Some(theme_str) = theme_str {
        theme_str
            .parse::<ThemeMode>()
            .map_err(|e| anyhow::anyhow!(e))?
    } else {
        ThemeMode::Dark
    };
    debug_log!("config: theme={theme:?}");

    // Determine UI type
    #[cfg(feature = "wayland")]
    let fuzzel_available = false;
    #[cfg(not(feature = "wayland"))]
    let fuzzel_available = find_binary("fuzzel");

    let ui_type = if let Some(ui_type_str) = ui_type_str {
        ui_type_str
            .parse::<UIType>()
            .map_err(|e| anyhow::anyhow!(e))?
    } else {
        default_ui_type(
            fuzzel_available,
            !general.fallbacks.is_empty() || general.main_view.is_some(),
        )?
    };

    if ui_type == UIType::Fuzzel && !general.fallbacks.is_empty() {
        eprintln!("Warning: fallbacks are configured but Fuzzel ignores them.");
    }
    if ui_type == UIType::Fuzzel && general.main_view.is_some() {
        eprintln!("Warning: main_view is configured but Fuzzel ignores it.");
    }
    let fuzzel_drops_aliases =
        ui_type == UIType::Fuzzel && parsed_config.entries.iter().any(|e| e.alias.is_some());

    debug_log!("config: ui_type={ui_type:?}");

    // Determine max history size
    let max_history = general.max_history.unwrap_or(10);

    // Determine font sizes (and proportional paddings)
    let mut font_sizes = if let Some(base) = general.font_size {
        ui::FontSizes::from_base(base)
    } else {
        ui::FontSizes::default_sizes()
    };

    // Override outer padding if explicitly set
    if let Some(padding) = general.padding {
        font_sizes.outer_padding = padding;
    }

    let (window_width, window_height) = resolve_window_dimensions(general);
    let opacity = resolve_opacity(&args, general);

    // Build UI settings
    let ui_settings = ui::UISettings {
        no_icons,
        initial_query: args.initial_query.clone(),
        select_1: args.select_1,
        app_name: args.app_name.clone(),
        theme,
        theme_colors: parsed_config.general.theme_colors.clone(),
        opacity,
        blur: general.blur.unwrap_or(true),
        renderer: resolve_renderer(general.renderer, opacity),
        max_history,
        font_sizes,
        font_family: general.font_family.clone(),
        window_width,
        window_height,
        sort_mode: general.sort_mode.clone(),
        fallbacks: general.fallbacks.clone(),
        main_view: general.main_view.clone(),
        main_view_chips: general.main_view_chips,
        clipboard_history: general.clipboard_history,
        terminal_classes: general.terminal_classes.clone(),
        show_preview: general.show_preview,
        show_tips: general.show_tips.unwrap_or(true),
        close_on_action: general.close_on_action,
        print_only: args.print_only,
        preview: general.preview.clone(),
        default_script_shell: default_script_shell.clone(),
    };

    debug_log!(
        "ui: settings: no_icons={} window={}x{} max_history={} sort_mode={:?}",
        ui_settings.no_icons,
        ui_settings.window_width,
        ui_settings.window_height,
        ui_settings.max_history,
        ui_settings.sort_mode
    );

    // Get the appropriate UI implementation
    if fuzzel_drops_aliases {
        eprintln!("Warning: launchers with alias are hidden with ui_type: fuzzel.");
        parsed_config.entries.retain(|entry| entry.alias.is_none());
    }
    let ui = ui::get_ui(ui_type);
    let chosen = ui
        .show(&parsed_config.entries, &parsed_config.addons, &ui_settings)
        .context("Failed to show UI")?;

    let chosen_name = chosen.trim();
    debug_log!("ui: chosen={chosen_name:?}");
    if chosen_name.is_empty() {
        std::process::exit(0);
    }
    let mc = find_chosen_entry(&parsed_config.entries, chosen_name)
        .context("No matching configuration found")?;

    let interpreter = if mc.script.is_some() {
        mc.binary.as_deref().unwrap_or(&default_script_shell)
    } else {
        // Not used for binary commands
        ""
    };
    debug_log!(
        "exec: binary={:?} script={} args={:?}",
        mc.binary,
        mc.script.is_some(),
        mc.args
    );
    execute_chosen_command(mc, &args, interpreter).context("Failed to execute command")?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::os::unix::fs::PermissionsExt;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_test_dir(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("raffi-{name}-{unique}"));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn script_config(script: &str, args: Option<Vec<&str>>) -> RaffiConfig {
        RaffiConfig {
            script: Some(script.to_string()),
            args: args.map(|args| args.iter().map(|arg| arg.to_string()).collect()),
            ..Default::default()
        }
    }

    fn binary_config(binary: Option<&str>, args: Option<Vec<&str>>) -> RaffiConfig {
        RaffiConfig {
            binary: binary.map(str::to_string),
            args: args.map(|args| args.iter().map(|arg| arg.to_string()).collect()),
            ..Default::default()
        }
    }

    #[test]
    fn test_print_only_script_without_args_has_no_dollar_zero() {
        let config = script_config("echo hello", None);
        assert_eq!(
            format_print_only_command(&config, "bash").unwrap(),
            "env -- bash -c 'echo hello'"
        );
    }

    #[test]
    fn test_print_only_script_with_empty_args_keeps_dollar_zero() {
        // Matches the exec path: Some(vec![]) still pushes the interpreter as $0.
        let config = script_config("echo hello", Some(vec![]));
        assert_eq!(
            format_print_only_command(&config, "bash").unwrap(),
            "env -- bash -c 'echo hello' bash"
        );
    }

    #[test]
    fn test_print_only_script_quotes_args() {
        let config = script_config("echo \"$1\"", Some(vec!["foo", "bar baz"]));
        assert_eq!(
            format_print_only_command(&config, "bash").unwrap(),
            "env -- bash -c 'echo \"$1\"' bash foo 'bar baz'"
        );
    }

    #[test]
    fn test_print_only_script_escapes_single_quotes() {
        let config = script_config("echo 'hi there'", None);
        assert_eq!(
            format_print_only_command(&config, "bash").unwrap(),
            r#"env -- bash -c 'echo '\''hi there'\'''"#
        );
    }

    #[test]
    fn test_print_only_script_preserves_newlines() {
        let config = script_config("echo one\necho two", None);
        assert_eq!(
            format_print_only_command(&config, "sh").unwrap(),
            "env -- sh -c 'echo one\necho two'"
        );
    }

    #[test]
    fn test_print_only_binary_with_safe_args_is_unquoted() {
        let config = binary_config(
            Some("/usr/bin/firefox"),
            Some(vec!["--new-window", "--width=100"]),
        );
        assert_eq!(
            format_print_only_command(&config, "").unwrap(),
            "/usr/bin/firefox --new-window --width=100"
        );
    }

    #[test]
    fn test_print_only_binary_quotes_unsafe_args() {
        let config = binary_config(Some("kitty"), Some(vec!["hello world", "; rm -rf /tmp/x"]));
        assert_eq!(
            format_print_only_command(&config, "").unwrap(),
            "env -- kitty 'hello world' '; rm -rf /tmp/x'"
        );
    }

    #[test]
    fn test_print_only_formats_command_position_words() {
        // Bare names are routed through env so a PATH lookup happens, as Command::new does.
        assert_eq!(format_command_word("firefox"), "env -- firefox");
        assert_eq!(format_command_word("source"), "env -- source");
        // Paths cannot be resolved as an alias, function, reserved word or builtin.
        assert_eq!(format_command_word("/usr/bin/firefox"), "/usr/bin/firefox");
        assert_eq!(format_command_word("./my app"), "'./my app'");
        // env would read this as an environment assignment, so quoting is used instead.
        assert_eq!(format_command_word("FOO=bar"), "'FOO=bar'");
        // Only command position is affected; arguments may safely contain '='.
        assert_eq!(shell_quote("--width=100"), "--width=100");
    }

    #[test]
    fn test_print_only_missing_binary_errors() {
        let config = binary_config(None, None);
        assert!(format_print_only_command(&config, "").is_err());
    }

    #[test]
    fn test_print_only_script_round_trips_through_shell() {
        let config = script_config(
            "printf '%s|%s|%s' \"$0\" \"$1\" \"$2\"",
            Some(vec!["first arg", "it's second"]),
        );
        let formatted = format_print_only_command(&config, "sh").unwrap();

        let output = Command::new("sh")
            .arg("-c")
            .arg(&formatted)
            .output()
            .expect("failed to run formatted command");

        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            "sh|first arg|it's second"
        );
    }

    #[test]
    fn test_print_only_binary_round_trips_through_shell() {
        let config = binary_config(Some("printf"), Some(vec!["%s|%s", "hello world", "a'b"]));
        let formatted = format_print_only_command(&config, "").unwrap();

        let output = Command::new("sh")
            .arg("-c")
            .arg(&formatted)
            .output()
            .expect("failed to run formatted command");

        assert_eq!(String::from_utf8_lossy(&output.stdout), "hello world|a'b");
    }

    /// Put `dir` ahead of the standard directories so its executables shadow the real ones.
    fn shadowed_path(dir: &Path) -> String {
        format!("{}:/usr/bin:/bin", dir.display())
    }

    /// Write an executable that prints each argument it receives on its own line.
    fn write_argv_printer(dir: &Path, name: &str) {
        let path = dir.join(name);
        fs::write(
            &path,
            "#!/bin/sh\nprintf 'EXTERNAL\\n'\nfor a in \"$@\"; do printf '[%s]\\n' \"$a\"; done\n",
        )
        .unwrap();
        let mut perms = fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&path, perms).unwrap();
    }

    /// A `binary` that collides with a shell builtin must still run the external program, the way
    /// `Command::new` does, and its arguments must never be evaluated as shell syntax.
    #[test]
    fn test_print_only_binary_shadowing_builtin_runs_external_program() {
        let dir = temp_test_dir("builtin-collision");
        write_argv_printer(&dir, "printf");

        let config = binary_config(Some("printf"), Some(vec!["$(id -u)", "a b; echo pwned"]));
        let formatted = format_print_only_command(&config, "").unwrap();

        let output = Command::new("/bin/sh")
            .arg("-c")
            .arg(&formatted)
            .env("PATH", shadowed_path(&dir))
            .output()
            .expect("failed to run formatted command");
        let stdout = String::from_utf8_lossy(&output.stdout);

        assert_eq!(stdout, "EXTERNAL\n[$(id -u)]\n[a b; echo pwned]\n");
        assert!(!stdout.contains("pwned\n[") && !stdout.ends_with("pwned\n"));

        let _ = fs::remove_dir_all(&dir);
    }

    /// `source` is a builtin in Bash but not in every shell, which is why the rule cannot rely on a
    /// list of builtin names.
    #[test]
    fn test_print_only_binary_shadowing_shell_specific_builtin_runs_external_program() {
        let dir = temp_test_dir("source-collision");
        write_argv_printer(&dir, "source");

        let config = binary_config(Some("source"), Some(vec!["/etc/hostname"]));
        let formatted = format_print_only_command(&config, "").unwrap();

        let output = Command::new("/bin/sh")
            .arg("-c")
            .arg(&formatted)
            .env("PATH", shadowed_path(&dir))
            .output()
            .expect("failed to run formatted command");

        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            "EXTERNAL\n[/etc/hostname]\n"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// A shell function defined in the evaluating shell must not take over from the real program.
    #[test]
    fn test_print_only_binary_shadowed_by_shell_function_runs_external_program() {
        let dir = temp_test_dir("function-collision");
        write_argv_printer(&dir, "raffifakeapp");

        let config = binary_config(Some("raffifakeapp"), Some(vec!["one"]));
        let formatted = format_print_only_command(&config, "").unwrap();

        let output = Command::new("/bin/sh")
            .arg("-c")
            .arg(format!(
                "raffifakeapp() {{ printf 'FUNCTION\\n'; }}\n{formatted}"
            ))
            .env("PATH", shadowed_path(&dir))
            .output()
            .expect("failed to run formatted command");

        assert_eq!(String::from_utf8_lossy(&output.stdout), "EXTERNAL\n[one]\n");

        let _ = fs::remove_dir_all(&dir);
    }

    /// The same protection applies to a script interpreter whose name collides with a builtin.
    #[test]
    fn test_print_only_script_interpreter_shadowing_builtin_runs_external_program() {
        let dir = temp_test_dir("interpreter-collision");
        write_argv_printer(&dir, "test");

        let config = script_config("echo hello", None);
        let formatted = format_print_only_command(&config, "test").unwrap();

        let output = Command::new("/bin/sh")
            .arg("-c")
            .arg(&formatted)
            .env("PATH", shadowed_path(&dir))
            .output()
            .expect("failed to run formatted command");

        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            "EXTERNAL\n[-c]\n[echo hello]\n"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_read_config_from_reader() {
        let yaml_config = r#"
        shell:
          binary: sh
          description: "Shell"
        hello_script:
          script: "echo hello"
          description: "Hello script"
        "#;
        let reader = Cursor::new(yaml_config);
        let args = Args {
            help: false,
            version: false,
            configfile: None,
            print_only: false,
            refresh_cache: false,
            no_icons: true,
            default_script_shell: None,
            ui_type: None,
            app_name: None,
            initial_query: None,
            select_1: false,
            theme: None,
            opacity: None,
            generate_schemas: false,
            check_config: false,
            debug_file: None,
            install_browser_host: false,
            uninstall_browser_host: false,
            force: false,
            keep_open: false,
            options: Vec::new(),
        };
        let parsed_config = read_config_from_reader(reader, &args).unwrap();
        assert_eq!(parsed_config.entries.len(), 2);

        // Addons should default to enabled
        assert!(parsed_config.addons.currency.enabled);
        assert!(parsed_config.addons.calculator.enabled);

        let expected_configs = vec![
            RaffiConfig {
                binary: Some("sh".to_string()),
                description: Some("Shell".to_string()),
                ..Default::default()
            },
            RaffiConfig {
                description: Some("Hello script".to_string()),
                script: Some("echo hello".to_string()),
                ..Default::default()
            },
        ];

        for expected_config in &expected_configs {
            assert!(parsed_config.entries.contains(expected_config));
        }
    }

    #[test]
    fn test_read_config_keeps_launcher_file_order() {
        let names = [
            "zulu", "alpha", "mike", "bravo", "yankee", "charlie", "xray", "delta", "whiskey",
            "echo",
        ];
        let launchers: String = names
            .iter()
            .map(|name| format!("  {name}:\n    script: \"true\"\n    description: \"{name}\"\n"))
            .collect();
        let yaml_config = format!("version: 1\nlaunchers:\n{launchers}");
        let parsed_config =
            read_config_from_reader(Cursor::new(yaml_config), &Args::default()).unwrap();
        let descriptions: Vec<_> = parsed_config
            .entries
            .iter()
            .map(|entry| entry.description.as_deref().unwrap_or_default())
            .collect();
        assert_eq!(descriptions, names);
    }

    #[test]
    fn test_addons_config_parsing() {
        let yaml_config = r#"
        addons:
          currency:
            enabled: true
            currencies: ["USD", "EUR", "GBP"]
          calculator:
            enabled: false
        shell:
          binary: sh
          description: "Shell"
        "#;
        let reader = Cursor::new(yaml_config);
        let args = Args {
            help: false,
            version: false,
            configfile: None,
            print_only: false,
            refresh_cache: false,
            no_icons: true,
            default_script_shell: None,
            ui_type: None,
            app_name: None,
            initial_query: None,
            select_1: false,
            theme: None,
            opacity: None,
            generate_schemas: false,
            check_config: false,
            debug_file: None,
            install_browser_host: false,
            uninstall_browser_host: false,
            force: false,
            keep_open: false,
            options: Vec::new(),
        };
        let parsed_config = read_config_from_reader(reader, &args).unwrap();

        assert!(parsed_config.addons.currency.enabled);
        assert!(!parsed_config.addons.calculator.enabled);
        assert_eq!(
            parsed_config.addons.currency.currencies,
            Some(vec![
                "USD".to_string(),
                "EUR".to_string(),
                "GBP".to_string()
            ])
        );
        assert_eq!(parsed_config.entries.len(), 1);
    }

    struct MockEnvProvider {
        vars: HashMap<String, String>,
    }

    impl EnvProvider for MockEnvProvider {
        fn var(&self, key: &str) -> Result<String, std::env::VarError> {
            self.vars
                .get(key)
                .cloned()
                .ok_or(std::env::VarError::NotPresent)
        }
    }

    struct MockBinaryChecker {
        binaries: Vec<String>,
    }

    impl BinaryChecker for MockBinaryChecker {
        fn exists(&self, binary: &str) -> bool {
            self.binaries.contains(&binary.to_string())
        }
    }

    #[test]
    fn test_is_valid_config() {
        let mut config = RaffiConfig {
            binary: Some("test-binary".to_string()),
            description: Some("Test Description".to_string()),
            script: None,
            args: None,
            icon: None,
            ifenveq: Some(vec!["TEST_VAR".to_string(), "true".to_string()]),
            ifenvset: Some("ANOTHER_VAR".to_string()),
            ifenvnotset: Some("MISSING_VAR".to_string()),
            ifexist: Some("another-binary".to_string()),
            disabled: None,
            show_preview: None,
            alias: None,
            keyword: None,
            close_on_action: None,
        };
        let args = Args {
            help: false,
            version: false,
            configfile: None,
            print_only: false,
            refresh_cache: false,
            no_icons: true,
            default_script_shell: None,
            ui_type: None,
            app_name: None,
            initial_query: None,
            select_1: false,
            theme: None,
            opacity: None,
            generate_schemas: false,
            check_config: false,
            debug_file: None,
            install_browser_host: false,
            uninstall_browser_host: false,
            force: false,
            keep_open: false,
            options: Vec::new(),
        };
        let env_provider = MockEnvProvider {
            vars: {
                let mut vars = HashMap::new();
                vars.insert("TEST_VAR".to_string(), "true".to_string());
                vars.insert("ANOTHER_VAR".to_string(), "some_value".to_string());
                vars
            },
        };
        let binary_checker = MockBinaryChecker {
            binaries: vec!["test-binary".to_string(), "another-binary".to_string()],
        };

        assert!(is_valid_config(
            &mut config,
            &args,
            &env_provider,
            &binary_checker
        ));
    }

    #[test]
    fn test_alias_launchers() {
        let yaml_config = r#"
        version: 1
        launchers:
          volup:
            description: Volume up
            alias: "audio volume up"
          untitled:
            alias: "audio "
          empty:
            description: Empty
            alias: "  "
          both:
            description: Both
            binary: sh
            alias: "audio volume up"
          with_args:
            description: With args
            alias: "audio volume up"
            args: ["-x"]
          shell:
            binary: sh
            description: Volume up
          linoux:
            description: Linus Pull Requests
            alias: "mp linus/linux"
            keyword: linoux
            icon: github
          linoux_again:
            description: Linus again
            alias: "mp other/repo"
            keyword: linoux
          spaced:
            description: Spaced
            alias: "mp x"
            keyword: "p c"
          no_alias:
            binary: sh
            description: No alias
            keyword: na
        "#;
        let parsed = read_config_from_reader(Cursor::new(yaml_config), &Args::default()).unwrap();
        let entries: Vec<(Option<&str>, Option<&str>, Option<&str>)> = parsed
            .entries
            .iter()
            .map(|e| {
                (
                    e.description.as_deref(),
                    e.binary.as_deref(),
                    e.alias.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            entries,
            vec![
                (Some("Volume up"), None, Some("audio volume up")),
                (Some("audio "), None, Some("audio ")),
                (Some("Volume up"), Some("sh"), None),
                (Some("Linus Pull Requests"), None, Some("mp linus/linux")),
                (Some("Linus again"), None, Some("mp other/repo")),
            ]
        );
        // A keyword used twice keeps the first launcher.
        assert_eq!(
            parsed.addons.launcher_keywords,
            vec![LauncherKeyword {
                keyword: "linoux".into(),
                alias: "mp linus/linux".into(),
                description: "Linus Pull Requests".into(),
                icon: Some("github".into()),
            }]
        );
        // A launcher sharing an alias's description still resolves to itself.
        let chosen = find_chosen_entry(&parsed.entries, "Volume up").unwrap();
        assert_eq!(chosen.binary.as_deref(), Some("sh"));
    }

    #[test]
    fn test_script_filter_action_parsing() {
        let yaml_config = r#"
        addons:
          script_filters:
            - name: "Bookmarks"
              keyword: "bm"
              command: "my-bookmark-script"
              args: ["-j"]
              action: "wl-copy {value}"
              secondary_action: "xdg-open {value}"
            - name: "Timezones"
              keyword: "tz"
              command: "batz"
              args: ["-j"]
        shell:
          binary: sh
          description: "Shell"
        "#;
        let reader = Cursor::new(yaml_config);
        let args = Args {
            help: false,
            version: false,
            configfile: None,
            print_only: false,
            refresh_cache: false,
            no_icons: true,
            default_script_shell: None,
            ui_type: None,
            app_name: None,
            initial_query: None,
            select_1: false,
            theme: None,
            opacity: None,
            generate_schemas: false,
            check_config: false,
            debug_file: None,
            install_browser_host: false,
            uninstall_browser_host: false,
            force: false,
            keep_open: false,
            options: Vec::new(),
        };
        let parsed_config = read_config_from_reader(reader, &args).unwrap();

        assert_eq!(parsed_config.addons.script_filters.len(), 2);

        let bm = &parsed_config.addons.script_filters[0];
        assert_eq!(bm.name, "Bookmarks");
        assert_eq!(bm.action, Some("wl-copy {value}".to_string()));
        assert_eq!(bm.secondary_action, Some("xdg-open {value}".to_string()));

        let tz = &parsed_config.addons.script_filters[1];
        assert_eq!(tz.name, "Timezones");
        assert_eq!(tz.action, None);
        assert_eq!(tz.secondary_action, None);
    }

    #[test]
    fn test_file_browser_sort_parsing() {
        let parse = |s: &str| serde_yaml::from_str::<FileBrowserSort>(s).unwrap();
        assert_eq!(parse("name"), FileBrowserSort::Name);
        assert_eq!(parse("modified"), FileBrowserSort::Modified);
        assert_eq!(parse("date"), FileBrowserSort::Modified);
        assert_eq!(parse("created"), FileBrowserSort::Created);
        assert!(serde_yaml::from_str::<FileBrowserSort>("size").is_err());
        assert_eq!(
            FileBrowserAddonConfig::default().sort,
            FileBrowserSort::Name
        );
        assert_eq!(FileBrowserSort::Created.next(), FileBrowserSort::Name);
    }

    #[test]
    fn test_file_browser_show_parsing() {
        let parse = |s: &str| serde_yaml::from_str::<FileBrowserShow>(s).unwrap();
        assert_eq!(parse("all"), FileBrowserShow::All);
        assert_eq!(parse("files"), FileBrowserShow::Files);
        assert_eq!(parse("directories"), FileBrowserShow::Directories);
        assert_eq!(parse("dirs"), FileBrowserShow::Directories);
        assert_eq!(parse("directory"), FileBrowserShow::Directories);
        assert!(serde_yaml::from_str::<FileBrowserShow>("links").is_err());
        assert_eq!(FileBrowserAddonConfig::default().show, FileBrowserShow::All);
        assert_eq!(FileBrowserShow::All.cycle(false), FileBrowserShow::Files);
        assert_eq!(
            FileBrowserShow::Files.cycle(false),
            FileBrowserShow::Directories
        );
        assert_eq!(
            FileBrowserShow::Directories.cycle(false),
            FileBrowserShow::All
        );
        assert_eq!(
            FileBrowserShow::All.cycle(true),
            FileBrowserShow::Directories
        );
    }

    #[test]
    fn test_text_snippet_action_parsing() {
        let yaml_config = r#"
        addons:
          text_snippets:
            - name: "Emails"
              keyword: "em"
              action: "wl-copy {value}"
              secondary_action: "wtype {value}"
              snippets:
                - name: "Personal"
                  value: "user@example.com"
            - name: "Plain"
              keyword: "pl"
              snippets:
                - name: "Hello"
                  value: "hello"
        shell:
          binary: sh
          description: "Shell"
        "#;
        let reader = Cursor::new(yaml_config);
        let args = Args {
            help: false,
            version: false,
            configfile: None,
            print_only: false,
            refresh_cache: false,
            no_icons: true,
            default_script_shell: None,
            ui_type: None,
            app_name: None,
            initial_query: None,
            select_1: false,
            theme: None,
            opacity: None,
            generate_schemas: false,
            check_config: false,
            debug_file: None,
            install_browser_host: false,
            uninstall_browser_host: false,
            force: false,
            keep_open: false,
            options: Vec::new(),
        };
        let parsed_config = read_config_from_reader(reader, &args).unwrap();

        assert_eq!(parsed_config.addons.text_snippets.len(), 2);

        let em = &parsed_config.addons.text_snippets[0];
        assert_eq!(em.name, "Emails");
        assert_eq!(em.action, Some("wl-copy {value}".to_string()));
        assert_eq!(em.secondary_action, Some("wtype {value}".to_string()));

        let pl = &parsed_config.addons.text_snippets[1];
        assert_eq!(pl.name, "Plain");
        assert_eq!(pl.action, None);
        assert_eq!(pl.secondary_action, None);
    }

    #[test]
    fn test_expand_tilde() {
        let home = std::env::var("HOME").unwrap();
        assert_eq!(expand_tilde("~/foo/bar"), format!("{home}/foo/bar"));
    }

    #[test]
    fn test_expand_tilde_no_tilde() {
        assert_eq!(expand_tilde("/usr/bin/foo"), "/usr/bin/foo");
        assert_eq!(expand_tilde("relative/path"), "relative/path");
    }

    #[test]
    fn test_expand_env_vars() {
        let home = std::env::var("HOME").unwrap();
        assert_eq!(expand_env_vars("${HOME}/foo"), format!("{home}/foo"));
    }

    #[test]
    fn test_expand_env_vars_unknown() {
        assert_eq!(expand_env_vars("${NONEXISTENT_VAR_12345}/foo"), "/foo");
    }

    #[test]
    fn test_expand_env_vars_multiple() {
        let home = std::env::var("HOME").unwrap();
        let user = std::env::var("USER").unwrap_or_default();
        assert_eq!(
            expand_env_vars("${HOME}/stuff/${USER}/data"),
            format!("{home}/stuff/{user}/data")
        );
    }

    #[test]
    fn test_expand_env_vars_no_vars() {
        assert_eq!(expand_env_vars("/usr/bin/foo"), "/usr/bin/foo");
        assert_eq!(expand_env_vars("plain text"), "plain text");
    }

    #[test]
    fn test_expand_config_value_combined() {
        let home = std::env::var("HOME").unwrap();
        let user = std::env::var("USER").unwrap_or_default();
        assert_eq!(
            expand_config_value("~/foo/${USER}/bar"),
            format!("{home}/foo/{user}/bar")
        );
    }

    #[test]
    fn test_config_expands_env_vars_in_fields() {
        let home = std::env::var("HOME").unwrap();
        let yaml_config = r#"
        version: 1
        launchers:
          myapp:
            binary: "${HOME}/bin/myapp"
            description: "My App"
            args: ["${HOME}/Downloads/file.txt", "--verbose"]
            icon: "${HOME}/icons/myapp.png"
            ifexist: "${HOME}/bin/myapp"
        "#;
        let reader = Cursor::new(yaml_config);
        let config: super::Config = serde_yaml::from_reader(reader).expect("cannot parse config");
        let mut rafficonfigs = Vec::new();
        for value in config.launchers.values() {
            if value.is_mapping() {
                let mut mc: RaffiConfig = serde_yaml::from_value(value.clone()).unwrap();
                mc.binary = mc.binary.map(|s| expand_config_value(&s));
                mc.icon = mc.icon.map(|s| expand_config_value(&s));
                mc.ifexist = mc.ifexist.map(|s| expand_config_value(&s));
                mc.args = mc
                    .args
                    .map(|v| v.into_iter().map(|s| expand_config_value(&s)).collect());
                rafficonfigs.push(mc);
            }
        }

        assert_eq!(rafficonfigs.len(), 1);
        let mc = &rafficonfigs[0];
        assert_eq!(mc.binary, Some(format!("{home}/bin/myapp")));
        assert_eq!(mc.icon, Some(format!("{home}/icons/myapp.png")));
        assert_eq!(mc.ifexist, Some(format!("{home}/bin/myapp")));
        assert_eq!(
            mc.args,
            Some(vec![
                format!("{home}/Downloads/file.txt"),
                "--verbose".to_string()
            ])
        );
    }

    #[test]
    fn test_config_expands_tilde_in_fields() {
        let home = std::env::var("HOME").unwrap();
        let yaml_config = r#"
        version: 1
        launchers:
          myapp:
            binary: "~/bin/myapp"
            description: "My App"
            args: ["~/Downloads/file.txt", "--verbose"]
            icon: "~/icons/myapp.png"
            ifexist: "~/bin/myapp"
        "#;
        let reader = Cursor::new(yaml_config);
        // Parse and expand manually to avoid is_valid_config filtering out non-existent paths
        let config: super::Config = serde_yaml::from_reader(reader).expect("cannot parse config");
        let mut rafficonfigs = Vec::new();
        for value in config.launchers.values() {
            if value.is_mapping() {
                let mut mc: RaffiConfig = serde_yaml::from_value(value.clone()).unwrap();
                mc.binary = mc.binary.map(|s| expand_tilde(&s));
                mc.icon = mc.icon.map(|s| expand_tilde(&s));
                mc.ifexist = mc.ifexist.map(|s| expand_tilde(&s));
                mc.args = mc
                    .args
                    .map(|v| v.into_iter().map(|s| expand_tilde(&s)).collect());
                rafficonfigs.push(mc);
            }
        }

        assert_eq!(rafficonfigs.len(), 1);
        let mc = &rafficonfigs[0];
        assert_eq!(mc.binary, Some(format!("{home}/bin/myapp")));
        assert_eq!(mc.icon, Some(format!("{home}/icons/myapp.png")));
        assert_eq!(mc.ifexist, Some(format!("{home}/bin/myapp")));
        assert_eq!(
            mc.args,
            Some(vec![
                format!("{home}/Downloads/file.txt"),
                "--verbose".to_string()
            ])
        );
    }

    #[cfg(feature = "wayland")]
    #[test]
    fn test_default_ui_type_prefers_native_over_fuzzel() {
        assert_eq!(default_ui_type(true, false).unwrap(), UIType::Native);
        assert_eq!(default_ui_type(true, true).unwrap(), UIType::Native);
    }

    #[cfg(not(feature = "wayland"))]
    #[test]
    fn test_default_ui_type_uses_fuzzel_without_native_support() {
        assert_eq!(default_ui_type(true, false).unwrap(), UIType::Fuzzel);
        assert!(default_ui_type(false, false).is_err());
        assert!(default_ui_type(true, true).is_err());
    }

    #[test]
    fn test_general_config_parsing() {
        let yaml_config = r#"
        general:
          ui_type: native
          default_script_shell: zsh
          no_icons: true
        shell:
          binary: sh
          description: "Shell"
        "#;
        let reader = Cursor::new(yaml_config);
        let args = Args {
            help: false,
            version: false,
            configfile: None,
            print_only: false,
            refresh_cache: false,
            no_icons: false,
            default_script_shell: None,
            ui_type: None,
            app_name: None,
            initial_query: None,
            select_1: false,
            theme: None,
            opacity: None,
            generate_schemas: false,
            check_config: false,
            debug_file: None,
            install_browser_host: false,
            uninstall_browser_host: false,
            force: false,
            keep_open: false,
            options: Vec::new(),
        };
        let parsed_config = read_config_from_reader(reader, &args).unwrap();

        assert_eq!(parsed_config.general.ui_type, Some("native".to_string()));
        assert_eq!(
            parsed_config.general.default_script_shell,
            Some("zsh".to_string())
        );
        assert_eq!(parsed_config.general.no_icons, Some(true));
        assert_eq!(parsed_config.entries.len(), 1);
    }

    #[test]
    fn test_config_without_general_section() {
        let yaml_config = r#"
        shell:
          binary: sh
          description: "Shell"
        "#;
        let reader = Cursor::new(yaml_config);
        let args = Args {
            help: false,
            version: false,
            configfile: None,
            print_only: false,
            refresh_cache: false,
            no_icons: false,
            default_script_shell: None,
            ui_type: None,
            app_name: None,
            initial_query: None,
            select_1: false,
            theme: None,
            opacity: None,
            generate_schemas: false,
            check_config: false,
            debug_file: None,
            install_browser_host: false,
            uninstall_browser_host: false,
            force: false,
            keep_open: false,
            options: Vec::new(),
        };
        let parsed_config = read_config_from_reader(reader, &args).unwrap();

        assert!(parsed_config.general.ui_type.is_none());
        assert!(parsed_config.general.default_script_shell.is_none());
        assert!(parsed_config.general.no_icons.is_none());
        assert_eq!(parsed_config.entries.len(), 1);
    }

    #[test]
    fn test_general_clipboard_history_parsing() {
        let parse =
            |yaml: &str| serde_yaml::from_str::<GeneralConfig>(yaml).map(|g| g.clipboard_history);
        assert_eq!(parse("{}").unwrap(), ClipboardHistory::Auto);
        for (value, expected) in [
            ("auto", ClipboardHistory::Auto),
            ("cliphist", ClipboardHistory::Cliphist),
            ("copyq", ClipboardHistory::Copyq),
            ("clipman", ClipboardHistory::Clipman),
            ("none", ClipboardHistory::None),
        ] {
            assert_eq!(
                parse(&format!("clipboard_history: {value}")).unwrap(),
                expected
            );
        }
        assert!(parse("clipboard_history: gpaste").is_err());
    }

    #[test]
    fn test_partial_general_config() {
        let yaml_config = r#"
        general:
          no_icons: true
        shell:
          binary: sh
          description: "Shell"
        "#;
        let reader = Cursor::new(yaml_config);
        let args = Args {
            help: false,
            version: false,
            configfile: None,
            print_only: false,
            refresh_cache: false,
            no_icons: false,
            default_script_shell: None,
            ui_type: None,
            app_name: None,
            initial_query: None,
            select_1: false,
            theme: None,
            opacity: None,
            generate_schemas: false,
            check_config: false,
            debug_file: None,
            install_browser_host: false,
            uninstall_browser_host: false,
            force: false,
            keep_open: false,
            options: Vec::new(),
        };
        let parsed_config = read_config_from_reader(reader, &args).unwrap();

        assert!(parsed_config.general.ui_type.is_none());
        assert!(parsed_config.general.default_script_shell.is_none());
        assert_eq!(parsed_config.general.no_icons, Some(true));
        assert_eq!(parsed_config.entries.len(), 1);
    }

    #[test]
    fn test_general_config_ui_settings_parsing() {
        let yaml_config = r#"
        general:
          font_size: 16
          font_family: "Inter"
          window_width: 900
          window_height: 500
        shell:
          binary: sh
          description: "Shell"
        "#;
        let reader = Cursor::new(yaml_config);
        let args = Args {
            help: false,
            version: false,
            configfile: None,
            print_only: false,
            refresh_cache: false,
            no_icons: false,
            default_script_shell: None,
            ui_type: None,
            app_name: None,
            initial_query: None,
            select_1: false,
            theme: None,
            opacity: None,
            generate_schemas: false,
            check_config: false,
            debug_file: None,
            install_browser_host: false,
            uninstall_browser_host: false,
            force: false,
            keep_open: false,
            options: Vec::new(),
        };
        let parsed_config = read_config_from_reader(reader, &args).unwrap();
        assert_eq!(parsed_config.general.font_size, Some(16.0));
        assert_eq!(parsed_config.general.font_family, Some("Inter".to_string()));
        assert_eq!(parsed_config.general.window_width, Some(900.0));
        assert_eq!(parsed_config.general.window_height, Some(500.0));
    }

    #[test]
    fn test_general_config_theme_parsing() {
        let yaml_config = r#"
        general:
          theme: light
        shell:
          binary: sh
          description: "Shell"
        "#;
        let reader = Cursor::new(yaml_config);
        let args = Args {
            help: false,
            version: false,
            configfile: None,
            print_only: false,
            refresh_cache: false,
            no_icons: false,
            default_script_shell: None,
            ui_type: None,
            app_name: None,
            initial_query: None,
            select_1: false,
            theme: None,
            opacity: None,
            generate_schemas: false,
            check_config: false,
            debug_file: None,
            install_browser_host: false,
            uninstall_browser_host: false,
            force: false,
            keep_open: false,
            options: Vec::new(),
        };
        let parsed_config = read_config_from_reader(reader, &args).unwrap();
        assert_eq!(parsed_config.general.theme, Some("light".to_string()));
    }

    #[test]
    fn test_config_window_dimensions_are_used() {
        let general = GeneralConfig {
            window_width: Some(700.0),
            window_height: Some(450.0),
            ..Default::default()
        };

        assert_eq!(resolve_window_dimensions(&general), (700.0, 450.0));
        assert_eq!(
            resolve_window_dimensions(&GeneralConfig::default()),
            (800.0, 600.0)
        );
    }

    #[test]
    fn test_window_geometry_flags_are_not_accepted() {
        for flag in ["--width", "--height", "--x", "--y"] {
            assert!(
                Args::parse_args_default::<&str>(&[flag, "100"]).is_err(),
                "{flag} should not be accepted"
            );
        }
    }

    #[test]
    fn test_opacity_resolution() {
        let general = GeneralConfig {
            opacity: Some(0.6),
            ..Default::default()
        };
        let args = Args::parse_args_default::<&str>(&["--opacity", "0.4"]).unwrap();
        assert_eq!(resolve_opacity(&args, &general), 0.4);
        assert_eq!(resolve_opacity(&Args::default(), &general), 0.6);
        assert_eq!(
            resolve_opacity(&Args::default(), &GeneralConfig::default()),
            DEFAULT_OPACITY
        );
        let too_high = GeneralConfig {
            opacity: Some(3.0),
            ..Default::default()
        };
        assert_eq!(resolve_opacity(&Args::default(), &too_high), 1.0);

        assert_eq!(resolve_renderer(Renderer::Auto, 0.85), Renderer::Gpu);
        assert_eq!(resolve_renderer(Renderer::Auto, 1.0), Renderer::Cpu);
        assert_eq!(resolve_renderer(Renderer::Cpu, 0.5), Renderer::Cpu);

        let parsed: GeneralConfig = serde_yaml::from_str("opacity: 0.7\nblur: false\n").unwrap();
        assert_eq!(parsed.opacity, Some(0.7));
        assert_eq!(parsed.blur, Some(false));
    }

    #[test]
    fn test_app_name_flag_is_parsed() {
        let args = Args::parse_args_default::<&str>(&["--app-name", "org.example.raffi"]).unwrap();
        assert_eq!(args.app_name.as_deref(), Some("org.example.raffi"));
    }

    #[test]
    fn test_theme_mode_from_str() {
        assert_eq!("dark".parse::<ThemeMode>().unwrap(), ThemeMode::Dark);
        assert_eq!("Dark".parse::<ThemeMode>().unwrap(), ThemeMode::Dark);
        assert_eq!("DARK".parse::<ThemeMode>().unwrap(), ThemeMode::Dark);
        assert_eq!("light".parse::<ThemeMode>().unwrap(), ThemeMode::Light);
        assert_eq!("Light".parse::<ThemeMode>().unwrap(), ThemeMode::Light);
        assert_eq!("LIGHT".parse::<ThemeMode>().unwrap(), ThemeMode::Light);
        assert_eq!(
            "catppuccin".parse::<ThemeMode>().unwrap(),
            ThemeMode::CatppuccinMocha
        );
        assert_eq!(
            "catppuccin-mocha".parse::<ThemeMode>().unwrap(),
            ThemeMode::CatppuccinMocha
        );
        assert_eq!(
            "catppuccin-latte".parse::<ThemeMode>().unwrap(),
            ThemeMode::CatppuccinLatte
        );
        assert_eq!(
            "tokyo-night".parse::<ThemeMode>().unwrap(),
            ThemeMode::TokyoNight
        );
        assert_eq!(
            "tokyonight".parse::<ThemeMode>().unwrap(),
            ThemeMode::TokyoNight
        );
        assert_eq!("nord".parse::<ThemeMode>().unwrap(), ThemeMode::Nord);
        assert_eq!("gruvbox".parse::<ThemeMode>().unwrap(), ThemeMode::Gruvbox);
        assert_eq!("dracula".parse::<ThemeMode>().unwrap(), ThemeMode::Dracula);
        assert_eq!(
            "rose-pine".parse::<ThemeMode>().unwrap(),
            ThemeMode::RosePine
        );
        assert_eq!(
            "rosepine".parse::<ThemeMode>().unwrap(),
            ThemeMode::RosePine
        );
        assert_eq!(
            "kanagawa".parse::<ThemeMode>().unwrap(),
            ThemeMode::Kanagawa
        );
        assert!(!ThemeMode::Dark.is_light());
        assert!(ThemeMode::Light.is_light());
        assert!(ThemeMode::CatppuccinLatte.is_light());
        assert!(!ThemeMode::CatppuccinMocha.is_light());
        assert!("invalid".parse::<ThemeMode>().is_err());
    }

    #[test]
    fn test_url_encode_query() {
        assert_eq!(url_encode_query("hello"), "hello");
        assert_eq!(url_encode_query("hello world"), "hello%20world");
        assert_eq!(url_encode_query("rust traits"), "rust%20traits");
        assert_eq!(url_encode_query("a+b"), "a%2Bb");
        assert_eq!(url_encode_query("foo&bar=baz"), "foo%26bar%3Dbaz");
        assert_eq!(url_encode_query(""), "");
        assert_eq!(url_encode_query("A-Z_0.9~"), "A-Z_0.9~");
    }

    #[test]
    fn test_web_search_config_parsing() {
        let yaml_config = r#"
        addons:
          web_searches:
            - name: "Google"
              keyword: "g"
              url: "https://google.com/search?q={query}"
              icon: "google"
            - name: "DuckDuckGo"
              keyword: "ddg"
              url: "https://duckduckgo.com/?q={query}"
        shell:
          binary: sh
          description: "Shell"
        "#;
        let reader = Cursor::new(yaml_config);
        let args = Args {
            help: false,
            version: false,
            configfile: None,
            print_only: false,
            refresh_cache: false,
            no_icons: true,
            default_script_shell: None,
            ui_type: None,
            app_name: None,
            initial_query: None,
            select_1: false,
            theme: None,
            opacity: None,
            generate_schemas: false,
            check_config: false,
            debug_file: None,
            install_browser_host: false,
            uninstall_browser_host: false,
            force: false,
            keep_open: false,
            options: Vec::new(),
        };
        let parsed_config = read_config_from_reader(reader, &args).unwrap();

        assert_eq!(parsed_config.addons.web_searches.len(), 2);

        let google = &parsed_config.addons.web_searches[0];
        assert_eq!(google.name, "Google");
        assert_eq!(google.keyword, "g");
        assert_eq!(google.url, "https://google.com/search?q={query}");
        assert_eq!(google.icon, Some("google".to_string()));

        let ddg = &parsed_config.addons.web_searches[1];
        assert_eq!(ddg.name, "DuckDuckGo");
        assert_eq!(ddg.keyword, "ddg");
        assert_eq!(ddg.url, "https://duckduckgo.com/?q={query}");
        assert!(ddg.icon.is_none());
    }

    #[test]
    fn test_text_snippet_config_parsing() {
        let yaml_config = r#"
        addons:
          text_snippets:
            - name: "Emails"
              keyword: "em"
              icon: "mail"
              snippets:
                - name: "Personal Email"
                  value: "user@example.com"
                  icon: "📨"
                - name: "Work Email"
                  value: "user@company.com"
                  keyword: "!work"
            - name: "Templates"
              keyword: "tpl"
              file: "~/.config/raffi/snippets.yaml"
              placeholders: false
              restore_clipboard: true
            - name: "Dynamic"
              keyword: "dyn"
              command: "my-snippet-gen"
              args: ["-j"]
        shell:
          binary: sh
          description: "Shell"
        "#;
        let reader = Cursor::new(yaml_config);
        let args = Args {
            help: false,
            version: false,
            configfile: None,
            print_only: false,
            refresh_cache: false,
            no_icons: true,
            default_script_shell: None,
            ui_type: None,
            app_name: None,
            initial_query: None,
            select_1: false,
            theme: None,
            opacity: None,
            generate_schemas: false,
            check_config: false,
            debug_file: None,
            install_browser_host: false,
            uninstall_browser_host: false,
            force: false,
            keep_open: false,
            options: Vec::new(),
        };
        let parsed_config = read_config_from_reader(reader, &args).unwrap();

        assert_eq!(parsed_config.addons.text_snippets.len(), 3);

        // Inline snippets source
        let emails = &parsed_config.addons.text_snippets[0];
        assert_eq!(emails.name, "Emails");
        assert_eq!(emails.keyword, "em");
        assert_eq!(emails.icon, Some("mail".to_string()));
        let snippets = emails.snippets.as_ref().unwrap();
        assert_eq!(snippets.len(), 2);
        assert_eq!(snippets[0].name, "Personal Email");
        assert_eq!(snippets[0].value, "user@example.com");
        assert_eq!(snippets[0].icon.as_deref(), Some("📨"));
        assert_eq!(snippets[1].icon, None);
        assert_eq!(snippets[1].name, "Work Email");
        assert_eq!(snippets[1].value, "user@company.com");
        assert_eq!(snippets[0].keyword, None);
        assert_eq!(snippets[1].keyword.as_deref(), Some("!work"));
        assert!(emails.placeholders);
        assert!(!emails.restore_clipboard);

        // File source
        let templates = &parsed_config.addons.text_snippets[1];
        assert_eq!(templates.name, "Templates");
        assert_eq!(templates.keyword, "tpl");
        let home = std::env::var("HOME").unwrap();
        assert_eq!(
            templates.file,
            Some(format!("{home}/.config/raffi/snippets.yaml"))
        );
        assert!(templates.snippets.is_none());
        assert!(templates.command.is_none());
        assert!(!templates.placeholders);
        assert!(templates.restore_clipboard);

        // Command source
        let dynamic = &parsed_config.addons.text_snippets[2];
        assert_eq!(dynamic.name, "Dynamic");
        assert_eq!(dynamic.keyword, "dyn");
        assert_eq!(dynamic.command, Some("my-snippet-gen".to_string()));
        assert_eq!(dynamic.args, vec!["-j".to_string()]);
        assert!(dynamic.snippets.is_none());
        assert!(dynamic.file.is_none());
    }

    #[test]
    fn test_text_snippet_directory_config_parsing() {
        let yaml_config = r#"
        addons:
          text_snippets:
            - name: "Snippets"
              keyword: "sn"
              icon: "snippets"
              directory: "~/.local/share/snippets"
        shell:
          binary: sh
          description: "Shell"
        "#;
        let reader = Cursor::new(yaml_config);
        let args = Args {
            help: false,
            version: false,
            configfile: None,
            print_only: false,
            refresh_cache: false,
            no_icons: true,
            default_script_shell: None,
            ui_type: None,
            app_name: None,
            initial_query: None,
            select_1: false,
            theme: None,
            opacity: None,
            generate_schemas: false,
            check_config: false,
            debug_file: None,
            install_browser_host: false,
            uninstall_browser_host: false,
            force: false,
            keep_open: false,
            options: Vec::new(),
        };
        let parsed_config = read_config_from_reader(reader, &args).unwrap();

        assert_eq!(parsed_config.addons.text_snippets.len(), 1);
        let snippets_dir = &parsed_config.addons.text_snippets[0];
        assert_eq!(snippets_dir.name, "Snippets");
        assert_eq!(snippets_dir.keyword, "sn");
        assert_eq!(snippets_dir.icon, Some("snippets".to_string()));
        let home = std::env::var("HOME").unwrap();
        assert_eq!(
            snippets_dir.directory,
            Some(format!("{home}/.local/share/snippets"))
        );
        assert!(snippets_dir.snippets.is_none());
        assert!(snippets_dir.file.is_none());
        assert!(snippets_dir.command.is_none());
    }

    #[test]
    fn test_text_snippet_defaults_to_empty() {
        let yaml_config = r#"
        shell:
          binary: sh
          description: "Shell"
        "#;
        let reader = Cursor::new(yaml_config);
        let args = Args {
            help: false,
            version: false,
            configfile: None,
            print_only: false,
            refresh_cache: false,
            no_icons: true,
            default_script_shell: None,
            ui_type: None,
            app_name: None,
            initial_query: None,
            select_1: false,
            theme: None,
            opacity: None,
            generate_schemas: false,
            check_config: false,
            debug_file: None,
            install_browser_host: false,
            uninstall_browser_host: false,
            force: false,
            keep_open: false,
            options: Vec::new(),
        };
        let parsed_config = read_config_from_reader(reader, &args).unwrap();
        assert!(parsed_config.addons.text_snippets.is_empty());
    }

    #[test]
    fn test_emoji_addon_config_parsing() {
        let yaml_config = r#"
        addons:
          emoji:
            enabled: true
            trigger: "em"
            action: "insert"
            secondary_action: "copy"
            data_files:
              - emojis_smileys_emotion
              - nerd_font
        shell:
          binary: sh
          description: "Shell"
        "#;
        let reader = Cursor::new(yaml_config);
        let args = Args {
            help: false,
            version: false,
            configfile: None,
            print_only: false,
            refresh_cache: false,
            no_icons: true,
            default_script_shell: None,
            ui_type: None,
            app_name: None,
            initial_query: None,
            select_1: false,
            theme: None,
            opacity: None,
            generate_schemas: false,
            check_config: false,
            debug_file: None,
            install_browser_host: false,
            uninstall_browser_host: false,
            force: false,
            keep_open: false,
            options: Vec::new(),
        };
        let parsed_config = read_config_from_reader(reader, &args).unwrap();

        assert!(parsed_config.addons.emoji.enabled);
        assert_eq!(parsed_config.addons.emoji.trigger, Some("em".to_string()));
        assert_eq!(
            parsed_config.addons.emoji.action,
            Some("insert".to_string())
        );
        assert_eq!(
            parsed_config.addons.emoji.secondary_action,
            Some("copy".to_string())
        );
        assert_eq!(
            parsed_config.addons.emoji.data_files,
            Some(vec![
                "emojis_smileys_emotion".to_string(),
                "nerd_font".to_string()
            ])
        );
    }

    #[test]
    fn test_clipboard_addon_config() {
        let parse = |yaml: &str| {
            let args = Args {
                help: false,
                version: false,
                configfile: None,
                print_only: false,
                refresh_cache: false,
                no_icons: true,
                default_script_shell: None,
                ui_type: None,
                app_name: None,
                initial_query: None,
                select_1: false,
                theme: None,
                opacity: None,
                generate_schemas: false,
                check_config: false,
                debug_file: None,
                install_browser_host: false,
                uninstall_browser_host: false,
                force: false,
                keep_open: false,
                options: Vec::new(),
            };
            read_config_from_reader(Cursor::new(yaml), &args)
        };

        let defaults = parse("shell:\n  binary: sh\n").unwrap().addons.clipboard;
        assert_eq!(defaults.enabled, None);
        assert_eq!(defaults.trigger(), "clip");
        assert_eq!(defaults.min_chars(), 3);
        assert_eq!(defaults.backend, ClipboardBackend::Copyq);
        assert_eq!(defaults.action, ClipboardAction::Copy);
        assert_eq!(defaults.secondary_action, ClipboardAction::HyprPaste);
        assert!(!defaults.uses_hypr_paste());

        let config = parse(
            r#"
addons:
  clipboard:
    enabled: true
    trigger: cb
    min_chars: 1
    backend: copyq
    action: hypr_paste
    secondary_action: copy
shell:
  binary: sh
"#,
        )
        .unwrap()
        .addons
        .clipboard;
        assert!(config.is_enabled());
        assert_eq!(config.trigger(), "cb");
        assert_eq!(config.min_chars(), 1);
        assert_eq!(config.action, ClipboardAction::HyprPaste);
        assert_eq!(config.secondary_action, ClipboardAction::Copy);
        assert!(config.uses_hypr_paste());

        assert!(parse("addons:\n  clipboard:\n    action: insert\n").is_err());
        assert!(parse("addons:\n  clipboard:\n    backend: cliphist\n").is_err());
    }

    #[test]
    fn test_devtools_addon_config_parsing() {
        let defaults = AddonsConfig::default().devtools;
        assert!(defaults.enabled);
        assert_eq!(defaults.trigger(), "dev");
        assert_eq!(defaults.password_length(), 20);

        let config: DevtoolsAddonConfig =
            serde_yaml::from_str("trigger: tx\npassword_length: 500\n").unwrap();
        assert_eq!(config.trigger(), "tx");
        assert_eq!(config.password_length(), 128);
    }

    #[test]
    fn test_windows_addon_config_parsing() {
        let defaults = AddonsConfig::default().windows;
        assert!(defaults.enabled);
        assert_eq!(defaults.trigger(), "win");

        let config: WindowsAddonConfig =
            serde_yaml::from_str("enabled: false\ntrigger: w\n").unwrap();
        assert!(!config.enabled);
        assert_eq!(config.trigger(), "w");
    }

    #[test]
    fn test_bluetooth_addon_config_parsing() {
        let defaults = AddonsConfig::default().bluetooth;
        assert!(defaults.enabled);
        assert_eq!(defaults.trigger(), "bt");
        assert!(!defaults.close_on_action(None));

        let config: BluetoothAddonConfig =
            serde_yaml::from_str("enabled: false\ntrigger: blue\nclose_on_action: true\n").unwrap();
        assert!(!config.enabled);
        assert_eq!(config.trigger(), "blue");
        assert!(config.close_on_action(None));
    }

    #[test]
    fn test_password_addon_config_parsing() {
        let defaults = AddonsConfig::default();
        assert!(defaults.pass.enabled);
        assert_eq!(defaults.pass.trigger(), "pass");
        assert_eq!(defaults.pass.pinentry, PassPinentry::Raffi);
        assert_eq!(
            defaults.pass.username_fields(),
            vec!["login", "user", "username"]
        );
        assert_eq!(defaults.pass.clipboard_timeout(), 45);
        assert_eq!(defaults.bitwarden.trigger(), "bw");

        let config: AddonsConfig = serde_yaml::from_str(
            "pass:\n  pinentry: agent\n  store_dir: ~/secrets\n  username_fields: [email]\n  clipboard_timeout: 0\nbitwarden:\n  trigger: vault\n  type_delay_ms: 500\n",
        )
        .unwrap();
        assert_eq!(config.pass.pinentry, PassPinentry::Agent);
        assert_eq!(config.pass.store_dir.as_deref(), Some("~/secrets"));
        assert_eq!(config.pass.username_fields(), vec!["email"]);
        assert_eq!(config.pass.clipboard_timeout(), 0);
        assert_eq!(config.bitwarden.trigger(), "vault");
        assert_eq!(config.bitwarden.type_delay_ms(), 500);
        assert!(serde_yaml::from_str::<PassAddonConfig>("pinentry: tty\n").is_err());
    }

    #[test]
    fn test_kill_addon_config_parsing() {
        let defaults = AddonsConfig::default().kill;
        assert!(defaults.enabled);
        assert_eq!(defaults.trigger(), "kill");

        let config: KillAddonConfig = serde_yaml::from_str("enabled: false\ntrigger: k\n").unwrap();
        assert!(!config.enabled);
        assert_eq!(config.trigger(), "k");
    }

    #[test]
    fn test_audio_addon_config_parsing() {
        let defaults = AddonsConfig::default().audio;
        assert!(defaults.enabled);
        assert_eq!(defaults.trigger(), "audio");
        assert!(!defaults.close_on_action(None));

        let config: AudioAddonConfig =
            serde_yaml::from_str("trigger: snd\nclose_on_action: true\n").unwrap();
        assert_eq!(config.trigger(), "snd");
        assert!(config.close_on_action(None));
    }

    #[test]
    fn test_bookmarks_addon_config_parsing() {
        let defaults = AddonsConfig::default().bookmarks;
        assert!(defaults.enabled);
        assert_eq!(defaults.trigger(), "bm");
        assert!(defaults.sources.is_none());

        let config: BookmarksAddonConfig = serde_yaml::from_str(
            "trigger: bk\nopen_with: brave\nsources:\n  - browser: firefox\n    profile: default-release\n    folder: Bookmarks Toolbar/Work\n    open_with: [qutebrowser, \"{url}\"]\n  - path: ~/Bookmarks\n",
        )
        .unwrap();
        assert_eq!(config.trigger(), "bk");
        assert_eq!(
            config.open_with,
            Some(BookmarkOpener::Browser(BookmarkBrowser::Brave))
        );
        let sources = config.sources.unwrap();
        assert_eq!(sources[0].browser, Some(BookmarkBrowser::Firefox));
        assert_eq!(sources[0].folder.as_deref(), Some("Bookmarks Toolbar/Work"));
        assert_eq!(
            sources[0].open_with,
            Some(BookmarkOpener::Command(vec![
                "qutebrowser".into(),
                "{url}".into()
            ]))
        );
        assert_eq!(sources[1].path.as_deref(), Some("~/Bookmarks"));
        assert!(serde_yaml::from_str::<BookmarksAddonConfig>("open_with: netscape\n").is_err());
    }

    #[test]
    fn test_browser_history_addon_config_parsing() {
        let defaults = AddonsConfig::default().browser_history;
        assert!(defaults.enabled);
        assert_eq!(defaults.trigger(), "hist");
        assert_eq!(defaults.max_entries(), 2000);
        assert!(defaults.sources.is_none());

        let config: BrowserHistoryAddonConfig = serde_yaml::from_str(
            "trigger: bh\nmax_entries: 5\nopen_with: brave\nsources:\n  - browser: firefox\n    profile: default-release\n    open_with: [qutebrowser, \"{url}\"]\n  - path: ~/History\n",
        )
        .unwrap();
        assert_eq!(config.trigger(), "bh");
        assert_eq!(config.max_entries(), 100);
        assert_eq!(
            config.open_with,
            Some(BookmarkOpener::Browser(BookmarkBrowser::Brave))
        );
        let sources = config.sources.unwrap();
        assert_eq!(sources[0].browser, Some(BookmarkBrowser::Firefox));
        assert_eq!(sources[0].profile.as_deref(), Some("default-release"));
        assert_eq!(
            sources[0].open_with,
            Some(BookmarkOpener::Command(vec![
                "qutebrowser".into(),
                "{url}".into()
            ]))
        );
        assert_eq!(sources[1].path.as_deref(), Some("~/History"));
        assert!(
            serde_yaml::from_str::<BrowserHistoryAddonConfig>("open_with: netscape\n").is_err()
        );
    }

    #[test]
    fn test_media_addon_config_parsing() {
        let defaults = AddonsConfig::default().media;
        assert!(defaults.enabled);
        assert_eq!(defaults.trigger(), "media");
        assert!(!defaults.close_on_action(None));

        let config: MediaAddonConfig =
            serde_yaml::from_str("trigger: mu\nclose_on_action: true\n").unwrap();
        assert_eq!(config.trigger(), "mu");
        assert!(config.close_on_action(None));
    }

    #[test]
    fn test_system_addon_config_parsing() {
        let defaults = AddonsConfig::default().system;
        assert!(defaults.enabled);
        assert_eq!(defaults.trigger(), "sys");

        let config: SystemAddonConfig = serde_yaml::from_str(
            "trigger: power\ncommands:\n  lock: [hyprlock]\nhide: [hibernate, logout]\n",
        )
        .unwrap();
        assert_eq!(config.trigger(), "power");
        assert_eq!(
            config.commands.get(SystemCommand::Lock),
            Some(&vec!["hyprlock".to_string()])
        );
        assert_eq!(config.commands.get(SystemCommand::Reboot), None);
        assert_eq!(
            config.hide,
            vec![SystemCommand::Hibernate, SystemCommand::Logout]
        );
    }

    #[test]
    fn test_emoji_addon_defaults_to_enabled() {
        let yaml_config = r#"
        shell:
          binary: sh
          description: "Shell"
        "#;
        let reader = Cursor::new(yaml_config);
        let args = Args {
            help: false,
            version: false,
            configfile: None,
            print_only: false,
            refresh_cache: false,
            no_icons: true,
            default_script_shell: None,
            ui_type: None,
            app_name: None,
            initial_query: None,
            select_1: false,
            theme: None,
            opacity: None,
            generate_schemas: false,
            check_config: false,
            debug_file: None,
            install_browser_host: false,
            uninstall_browser_host: false,
            force: false,
            keep_open: false,
            options: Vec::new(),
        };
        let parsed_config = read_config_from_reader(reader, &args).unwrap();

        // Emoji addon is enabled by default
        assert!(parsed_config.addons.emoji.enabled);
        assert!(parsed_config.addons.emoji.trigger.is_none());
        assert!(parsed_config.addons.emoji.action.is_none());
        assert!(parsed_config.addons.emoji.secondary_action.is_none());
        assert!(parsed_config.addons.emoji.data_files.is_none());
    }

    #[test]
    fn test_v0_config_migrated_in_memory() {
        // v0 format: launcher entries at top level (no version field)
        let yaml_config = r#"
        general:
          no_icons: true
        addons:
          calculator:
            enabled: false
        shell:
          binary: sh
          description: "Shell"
        firefox:
          binary: firefox
          description: "Firefox"
        "#;
        let reader = Cursor::new(yaml_config);
        let args = Args {
            help: false,
            version: false,
            configfile: None,
            print_only: false,
            refresh_cache: false,
            no_icons: true,
            default_script_shell: None,
            ui_type: None,
            app_name: None,
            initial_query: None,
            select_1: false,
            theme: None,
            opacity: None,
            generate_schemas: false,
            check_config: false,
            debug_file: None,
            install_browser_host: false,
            uninstall_browser_host: false,
            force: false,
            keep_open: false,
            options: Vec::new(),
        };
        let parsed_config = read_config_from_reader(reader, &args).unwrap();

        // general and addons should be preserved
        assert_eq!(parsed_config.general.no_icons, Some(true));
        assert!(!parsed_config.addons.calculator.enabled);

        // launcher entries should be found
        assert_eq!(parsed_config.entries.len(), 2);
    }

    #[test]
    fn test_v1_config_passes_through() {
        // v1 format: entries under launchers key
        let yaml_config = r#"
        version: 1
        general:
          no_icons: true
        launchers:
          shell:
            binary: sh
            description: "Shell"
          firefox:
            binary: firefox
            description: "Firefox"
        "#;
        let reader = Cursor::new(yaml_config);
        let args = Args {
            help: false,
            version: false,
            configfile: None,
            print_only: false,
            refresh_cache: false,
            no_icons: true,
            default_script_shell: None,
            ui_type: None,
            app_name: None,
            initial_query: None,
            select_1: false,
            theme: None,
            opacity: None,
            generate_schemas: false,
            check_config: false,
            debug_file: None,
            install_browser_host: false,
            uninstall_browser_host: false,
            force: false,
            keep_open: false,
            options: Vec::new(),
        };
        let parsed_config = read_config_from_reader(reader, &args).unwrap();

        assert_eq!(parsed_config.general.no_icons, Some(true));
        assert_eq!(parsed_config.entries.len(), 2);
    }

    #[test]
    fn test_migrate_v0_to_v1_moves_keys() {
        let yaml_str = r#"
        general:
          no_icons: true
        addons:
          calculator:
            enabled: false
        shell:
          binary: sh
          description: "Shell"
        firefox:
          binary: firefox
          description: "Firefox"
        "#;
        let mut raw: Value = serde_yaml::from_str(yaml_str).unwrap();

        // Create a temp file for migration
        let dir = std::env::temp_dir();
        let config_path = dir.join("test_migrate_v0.yaml");
        fs::write(&config_path, yaml_str).unwrap();

        let migrated = migrate_config_v0_to_v1(config_path.to_str().unwrap(), &mut raw).unwrap();
        assert!(migrated);

        // Check structure after migration
        let mapping = raw.as_mapping().unwrap();
        assert!(mapping.contains_key(Value::String("version".to_string())));
        assert!(mapping.contains_key(Value::String("launchers".to_string())));
        assert!(mapping.contains_key(Value::String("general".to_string())));
        assert!(mapping.contains_key(Value::String("addons".to_string())));

        // shell and firefox should NOT be top-level anymore
        assert!(!mapping.contains_key(Value::String("shell".to_string())));
        assert!(!mapping.contains_key(Value::String("firefox".to_string())));

        // They should be inside launchers
        let launchers = mapping
            .get(Value::String("launchers".to_string()))
            .unwrap()
            .as_mapping()
            .unwrap();
        assert!(launchers.contains_key(Value::String("shell".to_string())));
        assert!(launchers.contains_key(Value::String("firefox".to_string())));

        // Backup should exist
        let backup_path = format!("{}.bak", config_path.to_str().unwrap());
        assert!(Path::new(&backup_path).exists());

        // Cleanup
        let _ = fs::remove_file(&config_path);
        let _ = fs::remove_file(&backup_path);
    }

    #[test]
    fn test_migrate_v1_is_noop() {
        let yaml_str = r#"
        version: 1
        launchers:
          shell:
            binary: sh
        "#;
        let mut raw: Value = serde_yaml::from_str(yaml_str).unwrap();

        let dir = std::env::temp_dir();
        let config_path = dir.join("test_migrate_v1_noop.yaml");
        fs::write(&config_path, yaml_str).unwrap();

        let migrated = migrate_config_v0_to_v1(config_path.to_str().unwrap(), &mut raw).unwrap();
        assert!(!migrated);

        // No backup should be created
        let backup_path = format!("{}.bak", config_path.to_str().unwrap());
        assert!(!Path::new(&backup_path).exists());

        // Cleanup
        let _ = fs::remove_file(&config_path);
    }

    #[test]
    fn test_schema_generation() {
        let schema = generate_schema();
        let parsed: serde_json::Value = serde_json::from_str(&schema).unwrap();

        // Should be a valid JSON Schema
        assert_eq!(parsed["type"], "object");

        // Should have our top-level properties
        let props = parsed["properties"].as_object().unwrap();
        assert!(props.contains_key("version"));
        assert!(props.contains_key("general"));
        assert!(props.contains_key("addons"));
        assert!(props.contains_key("launchers"));
    }

    #[test]
    fn test_find_binary_accepts_paths() {
        let dir = temp_test_dir("find-binary");
        let binary = dir.join("custom-tool");
        fs::write(&binary, "#!/bin/sh\n").unwrap();

        assert!(find_binary(binary.to_str().unwrap()));

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn test_parse_desktop_exec_strips_field_codes() {
        let (binary, args) = parse_desktop_exec(
            r#"/usr/bin/env BAMF_DESKTOP_FILE_HINT=%k /usr/bin/firefox --new-window %U"#,
        )
        .unwrap();

        assert_eq!(binary, "/usr/bin/env");
        assert_eq!(
            args,
            vec![
                "BAMF_DESKTOP_FILE_HINT=".to_string(),
                "/usr/bin/firefox".to_string(),
                "--new-window".to_string()
            ]
        );
    }

    #[test]
    fn test_detect_desktop_entries_from_dirs() {
        let dir = temp_test_dir("desktop-detect");
        let apps_dir = dir.join("applications");
        fs::create_dir_all(&apps_dir).unwrap();

        fs::write(
            apps_dir.join("firefox.desktop"),
            r#"[Desktop Entry]
Type=Application
Name=Firefox
Exec=firefox %U
Icon=firefox
"#,
        )
        .unwrap();
        fs::write(
            apps_dir.join("hidden.desktop"),
            r#"[Desktop Entry]
Type=Application
Name=Hidden App
Exec=hidden-app
Hidden=true
"#,
        )
        .unwrap();
        fs::write(
            apps_dir.join("missing-tryexec.desktop"),
            r#"[Desktop Entry]
Type=Application
Name=Missing TryExec
Exec=missing-app
TryExec=missing-app
"#,
        )
        .unwrap();
        fs::write(
            apps_dir.join("terminal.desktop"),
            r#"[Desktop Entry]
Type=Application
Name=Terminal App
Exec=terminal-app
Terminal=true
"#,
        )
        .unwrap();

        let checker = MockBinaryChecker {
            binaries: vec!["firefox".to_string()],
        };
        let detected = detect_desktop_entries_from_dirs(&[apps_dir], &plain_scan(), &checker);

        assert_eq!(detected.len(), 1);
        assert_eq!(detected[0].description.as_deref(), Some("Firefox"));
        assert_eq!(detected[0].binary.as_deref(), Some("firefox"));
        assert!(detected[0].args.as_deref().unwrap_or(&[]).is_empty());

        let _ = fs::remove_dir_all(dir);
    }

    fn plain_scan() -> DesktopScan {
        DesktopScan {
            actions: false,
            terminal: None,
            desktops: Vec::new(),
        }
    }

    fn descriptions(entries: &[RaffiConfig]) -> Vec<&str> {
        entries
            .iter()
            .map(|entry| entry.description.as_deref().unwrap_or_default())
            .collect()
    }

    #[test]
    fn test_detect_desktop_entries_actions_desktops_and_terminal() {
        let dir = temp_test_dir("desktop-scan");
        let user_dir = dir.join("user/applications");
        let system_dir = dir.join("system/applications");
        fs::create_dir_all(&user_dir).unwrap();
        fs::create_dir_all(&system_dir).unwrap();
        let files = [
            (
                &system_dir,
                "firefox.desktop",
                "[Desktop Entry]\nType=Application\nName=Firefox\nExec=firefox %u\nIcon=firefox\n\
                 Actions=private;broken;\n\n[Desktop Action private]\nName=New Private Window\n\
                 Exec=firefox --private-window %u\n\n[Desktop Action broken]\nName=Broken\n",
            ),
            (
                &system_dir,
                "gnome-only.desktop",
                "[Desktop Entry]\nType=Application\nName=Gnome Only\nExec=firefox\nOnlyShowIn=GNOME;\n",
            ),
            (
                &system_dir,
                "not-hypr.desktop",
                "[Desktop Entry]\nType=Application\nName=Not Hyprland\nExec=firefox\nNotShowIn=Hyprland;\n",
            ),
            (
                &system_dir,
                "htop.desktop",
                "[Desktop Entry]\nType=Application\nName=Htop\nExec=htop -t\nTerminal=true\n",
            ),
            (
                &system_dir,
                "overridden.desktop",
                "[Desktop Entry]\nType=Application\nName=Overridden\nExec=firefox\n",
            ),
            (
                &user_dir,
                "overridden.desktop",
                "[Desktop Entry]\nType=Application\nName=Overridden\nExec=firefox\nNoDisplay=true\n",
            ),
        ];
        for (dir, name, contents) in files {
            fs::write(dir.join(name), contents).unwrap();
        }
        let checker = MockBinaryChecker {
            binaries: vec!["firefox".to_string(), "htop".to_string()],
        };
        let dirs = [user_dir, system_dir];

        let scan = DesktopScan {
            actions: true,
            terminal: Some(vec!["foot".to_string()]),
            desktops: vec!["hyprland".to_string()],
        };
        let detected = detect_desktop_entries_from_dirs(&dirs, &scan, &checker);
        assert_eq!(
            descriptions(&detected),
            ["Firefox", "Firefox: New Private Window", "Htop"]
        );
        assert_eq!(
            detected[1].args.as_deref(),
            Some(&["--private-window".to_string()][..])
        );
        assert_eq!(detected[1].icon.as_deref(), Some("firefox"));
        assert_eq!(detected[2].binary.as_deref(), Some("foot"));
        assert_eq!(
            detected[2].args.as_deref(),
            Some(&["htop".to_string(), "-t".to_string()][..])
        );

        let detected = detect_desktop_entries_from_dirs(&dirs, &plain_scan(), &checker);
        // OnlyShowIn hides an entry unless the desktop is listed.
        assert_eq!(descriptions(&detected), ["Firefox", "Not Hyprland"]);

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn test_merge_apps_skips_launcher_duplicates() {
        let launcher = |description: &str, binary: &str, args: Option<&[&str]>| RaffiConfig {
            description: Some(description.to_string()),
            binary: Some(binary.to_string()),
            args: args.map(|args| args.iter().map(|arg| arg.to_string()).collect()),
            ..Default::default()
        };
        let mut entries = vec![
            launcher("firefox", "firefox", None),
            launcher("Files", "nautilus", Some(&["--new-window"])),
        ];
        merge_apps(
            &mut entries,
            vec![
                launcher("Firefox", "/usr/bin/firefox", None),
                launcher("Nautilus", "nautilus", Some(&["--new-window"])),
                launcher("Nautilus: Open", "nautilus", None),
                launcher("Htop", "htop", Some(&[])),
            ],
        );
        assert_eq!(
            descriptions(&entries),
            ["firefox", "Files", "Nautilus: Open", "Htop"]
        );
    }

    #[test]
    fn test_desktop_entry_icon_map_from_dirs_uses_real_exec_binary() {
        let dir = temp_test_dir("desktop-icon-map");
        let apps_dir = dir.join("applications");
        fs::create_dir_all(&apps_dir).unwrap();

        fs::write(
            apps_dir.join("firefox.desktop"),
            r#"[Desktop Entry]
Type=Application
Name=Firefox
Exec=/usr/bin/env BAMF_DESKTOP_FILE_HINT=%k /usr/bin/firefox --new-window %U
Icon=firefox
"#,
        )
        .unwrap();

        let checker = MockBinaryChecker {
            binaries: vec![
                "/usr/bin/env".to_string(),
                "/usr/bin/firefox".to_string(),
                "firefox".to_string(),
            ],
        };
        let icon_map = desktop_entry_icon_map_from_dirs(&[apps_dir], &checker);

        assert_eq!(
            icon_map.get("/usr/bin/firefox"),
            Some(&"firefox".to_string())
        );
        assert_eq!(icon_map.get("firefox"), Some(&"firefox".to_string()));
        assert!(!icon_map.contains_key("/usr/bin/env"));
        assert!(!icon_map.contains_key("env"));

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn test_resolve_launcher_icon_path_uses_launcher_args_for_lookup() {
        let config = RaffiConfig {
            binary: Some("jumpapp".to_string()),
            args: Some(vec!["-X".to_string(), "firefox".to_string()]),
            description: Some("Firefox".to_string()),
            ..Default::default()
        };
        let icon_map = HashMap::from([("firefox".to_string(), "/icons/firefox.svg".to_string())]);

        let resolved = resolve_launcher_icon_path(&config, &icon_map, &HashMap::new());

        assert_eq!(resolved.as_deref(), Some("/icons/firefox.svg"));
    }

    #[test]
    fn test_resolve_launcher_icon_path_uses_desktop_icon_map() {
        let config = RaffiConfig {
            binary: Some("custom-launcher".to_string()),
            description: Some("Firefox".to_string()),
            ..Default::default()
        };
        let icon_map = HashMap::from([("firefox".to_string(), "/icons/firefox.svg".to_string())]);
        let desktop_icon_map =
            HashMap::from([("custom-launcher".to_string(), "firefox".to_string())]);

        let resolved = resolve_launcher_icon_path(&config, &icon_map, &desktop_icon_map);

        assert_eq!(resolved.as_deref(), Some("/icons/firefox.svg"));
    }

    #[test]
    fn test_resolve_icon_reference_ignores_non_icon_files() {
        let dir = temp_test_dir("resolve-icon-reference");
        let binary_path = dir.join("firefox");
        fs::write(&binary_path, "#!/bin/sh\n").unwrap();

        let icon_map = HashMap::from([("firefox".to_string(), "/icons/firefox.svg".to_string())]);
        let resolved = resolve_icon_reference(binary_path.to_str().unwrap(), &icon_map);

        assert_eq!(resolved.as_deref(), Some("/icons/firefox.svg"));

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn test_resolve_icon_reference_ignores_directories() {
        let dir = temp_test_dir("resolve-icon-directory");
        let resolved = resolve_icon_reference(dir.to_str().unwrap(), &HashMap::new());

        assert!(resolved.is_none());

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn test_default_icon_path_materializes_bundled_icon_without_repo_cwd() {
        let cache_root = temp_test_dir("default-icon-cache");
        let cwd = temp_test_dir("default-icon-cwd");
        let previous_cache_home = std::env::var_os("XDG_CACHE_HOME");
        let previous_cwd = std::env::current_dir().unwrap();

        std::env::set_var("XDG_CACHE_HOME", &cache_root);
        std::env::set_current_dir(&cwd).unwrap();

        let resolved = default_icon_path(&HashMap::new()).unwrap();
        let resolved_path = PathBuf::from(&resolved);

        assert!(resolved_path.exists());
        assert_eq!(
            resolved_path.file_name().and_then(|name| name.to_str()),
            Some("default_icon.svg")
        );
        assert_eq!(fs::read(&resolved_path).unwrap(), DEFAULT_ICON_BYTES);

        std::env::set_current_dir(previous_cwd).unwrap();
        match previous_cache_home {
            Some(value) => std::env::set_var("XDG_CACHE_HOME", value),
            None => std::env::remove_var("XDG_CACHE_HOME"),
        }

        let _ = fs::remove_dir_all(cache_root);
        let _ = fs::remove_dir_all(cwd);
    }

    #[cfg(feature = "config-validation")]
    fn yaml(s: &str) -> Value {
        serde_yaml::from_str(s).unwrap()
    }

    #[test]
    #[cfg(feature = "config-validation")]
    fn test_example_config_is_valid() {
        let mut raw = yaml(include_str!("../examples/raffi.yaml"));
        migrate_config_v0_in_memory(&mut raw);
        assert_eq!(validate_config_value(&raw), Vec::<ConfigProblem>::new());
    }

    #[test]
    fn test_example_schema_is_up_to_date() {
        assert_eq!(
            include_str!("../examples/raffi-schema.json").trim_end(),
            generate_schema(),
            "regenerate with: cargo run -- --generate-schemas --configfile examples/raffi.yaml"
        );
    }

    #[test]
    fn test_example_launchers_schema_is_up_to_date() {
        assert_eq!(
            include_str!("../examples/launchers-schema.json").trim_end(),
            generate_launchers_schema(),
            "regenerate with: cargo run -- --generate-schemas --configfile examples/raffi.yaml"
        );
    }

    #[test]
    fn test_generate_schemas_writes_both_next_to_config() {
        let dir = temp_test_dir("generate-schemas");
        let configfile = dir.join("raffi.yaml");
        run(Args {
            configfile: Some(configfile.to_string_lossy().into_owned()),
            generate_schemas: true,
            ..Args::default()
        })
        .unwrap();

        assert_eq!(
            fs::read_to_string(dir.join("raffi-schema.json")).unwrap(),
            generate_schema()
        );
        assert_eq!(
            fs::read_to_string(dir.join("launchers-schema.json")).unwrap(),
            generate_launchers_schema()
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    #[cfg(feature = "config-validation")]
    fn test_validate_reports_unknown_and_mistyped_keys() {
        let text = "version: 1\n\
             general:\n  them: dark\n\
             addons:\n  calculator:\n    foo: 1\n\
             launchers:\n  ff:\n    binnary: firefox\n    args: firefox\n";
        let mut problems = validate_config_value(&yaml(text));
        locate_config_problems(&mut problems, text, false);
        let has = |path: &str, needle: &str, line: usize| {
            problems
                .iter()
                .any(|p| p.path == path && p.message.contains(needle) && p.line == Some(line))
        };
        assert_eq!(problems.len(), 4, "{problems:?}");
        assert!(has("/general", "'them'", 3), "{problems:?}");
        assert!(has("/addons/calculator", "'foo'", 6), "{problems:?}");
        assert!(has("/launchers/ff", "'binnary'", 9), "{problems:?}");
        assert!(has("/launchers/ff/args", "array", 10), "{problems:?}");
    }

    #[test]
    #[cfg(feature = "config-validation")]
    fn test_validate_accepts_file_browser_show_aliases() {
        for show in ["all", "files", "directories", "dirs", "directory"] {
            let raw = yaml(&format!(
                "version: 1\naddons:\n  file_browser:\n    show: {show}\n"
            ));
            assert_eq!(validate_config_value(&raw), Vec::<ConfigProblem>::new());
        }
        let raw = yaml("version: 1\naddons:\n  file_browser:\n    show: links\n");
        assert_eq!(validate_config_value(&raw).len(), 1);
    }

    #[test]
    #[cfg(feature = "config-validation")]
    fn test_validate_v0_config_after_migration() {
        let text = "firefox:\n  binary: firefox\n  descr: x\n";
        let mut raw = yaml(text);
        let v0 = migrate_config_v0_in_memory(&mut raw);
        let mut problems = validate_config_value(&raw);
        locate_config_problems(&mut problems, text, v0);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert_eq!(problems[0].path, "/launchers/firefox");
        assert_eq!(problems[0].line, Some(3));
    }

    #[test]
    fn test_find_yaml_line() {
        let text = "\
# comment
version: 1
addons:
  script_filters:
  - name: one
    command: a
  - name: \"two\"
    command: |
      echo a: b
    args:
      - x
      - y
  web_searches: []
launchers:
  \"my/app\":
    binary: foo # trailing
";
        let cases = [
            ("/version", Some(2)),
            ("/addons", Some(3)),
            ("/addons/script_filters/0", Some(5)),
            ("/addons/script_filters/0/name", Some(5)),
            ("/addons/script_filters/0/command", Some(6)),
            ("/addons/script_filters/1/command", Some(8)),
            ("/addons/script_filters/1/args/1", Some(12)),
            ("/addons/web_searches", Some(13)),
            ("/addons/web_searches/0", Some(13)),
            ("/launchers/my~1app/binary", Some(16)),
            ("/launchers/missing", Some(14)),
            ("/nope", None),
            ("/", None),
        ];
        for (pointer, line) in cases {
            assert_eq!(find_yaml_line(text, pointer), line, "{pointer}");
        }
    }

    #[test]
    #[cfg(feature = "config-validation")]
    fn test_check_config_reports_lines() {
        let dir = temp_test_dir("check-config-lines");
        let path = dir.join("raffi.yaml");
        fs::write(
            &path,
            "version: 1\nlaunchers:\n  ff:\n    binary: firefox\n    ifexist: 1\n",
        )
        .unwrap();

        let problems = check_config(path.to_str().unwrap()).unwrap();

        assert_eq!(problems.len(), 1, "{problems:?}");
        assert_eq!(problems[0].line, Some(5));
        let shown = format_config_problem("raffi.yaml", &problems[0]);
        assert!(
            shown.starts_with("raffi.yaml:5: /launchers/ff/ifexist: "),
            "{shown}"
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    #[cfg(feature = "config-validation")]
    fn test_check_launchers_file() {
        let example = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/launchers.yaml");
        assert_eq!(check_launchers_file(&example).unwrap(), Vec::new());

        let dir = temp_test_dir("check-launchers-file");
        let path = dir.join("launchers.yaml");
        fs::write(&path, "ff:\n  binary: firefox\n  ifexist: 1\n").unwrap();
        let problems = check_launchers_file(&path).unwrap();
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert_eq!(problems[0].path, "/ff/ifexist");
        assert_eq!(problems[0].line, Some(3));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn test_read_config_uses_launchers_file() {
        let dir = temp_test_dir("launchers-file");
        let config = dir.join("raffi.yaml");
        let config = config.to_str().unwrap();
        fs::write(dir.join("launchers.yaml"), "from-file:\n  binary: sh\n").unwrap();
        let args = Args::parse_args_default::<&str>(&[]).unwrap();

        fs::write(config, "version: 1\ngeneral:\n  theme: light\n").unwrap();
        let parsed = read_config(config, &args).unwrap();
        assert_eq!(parsed.general.theme.as_deref(), Some("light"));
        assert_eq!(parsed.entries.len(), 1);
        assert_eq!(parsed.entries[0].binary.as_deref(), Some("sh"));

        fs::write(
            config,
            "version: 1\nlaunchers:\n  inline:\n    binary: sh\n    description: inline\n",
        )
        .unwrap();
        let parsed = read_config(config, &args).unwrap();
        assert_eq!(parsed.entries.len(), 1);
        assert_eq!(parsed.entries[0].description.as_deref(), Some("inline"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn test_unknown_keys_still_load() {
        let config = "version: 1\nlaunchers:\n  sh:\n    binary: sh\n    binnary: typo\n";
        let args = Args::parse_args_default::<&str>(&[]).unwrap();
        let parsed = read_config_from_reader(Cursor::new(config), &args).unwrap();
        assert_eq!(parsed.entries.len(), 1);
        assert_eq!(parsed.entries[0].binary.as_deref(), Some("sh"));
    }

    #[test]
    #[cfg(feature = "config-validation")]
    fn test_check_config_does_not_rewrite_v0_file() {
        let dir = temp_test_dir("check-config");
        let path = dir.join("raffi.yaml");
        let contents = "firefox:\n  binary: firefox\n";
        fs::write(&path, contents).unwrap();

        let problems = check_config(path.to_str().unwrap()).unwrap();

        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(fs::read_to_string(&path).unwrap(), contents);
        assert!(!dir.join("raffi.yaml.bak").exists());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn test_write_schema_if_changed_refreshes_stale_file() {
        let dir = temp_test_dir("schema-refresh");
        let path = dir.join("raffi-schema.json");

        write_schema_if_changed(&path, &generate_schema()).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), generate_schema());

        fs::write(&path, "{}").unwrap();
        write_schema_if_changed(&path, &generate_schema()).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), generate_schema());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn test_cli_overrides_and_keep_open() {
        let yaml_config = r#"
version: 1
general:
  theme: dark
addons:
  script_filters:
    - name: Timezones
      keyword: tz
      command: echo
  web_searches:
    - name: Google
      keyword: g
      url: "https://google.com?q={query}"
launchers:
  shell:
    binary: sh
    description: "Shell"
"#;
        let args = Args {
            keep_open: true,
            options: vec![
                "general.theme=nord".into(),
                "addons.emoji.close_on_action=true".into(),
                "addons.script_filters.Timezones.close_on_action=false".into(),
                "addons.web_searches.g.close_on_action=true".into(),
                "launchers.shell.close_on_action=true".into(),
            ],
            ..Args::default()
        };
        let parsed = read_config_from_reader(Cursor::new(yaml_config), &args).unwrap();
        assert_eq!(parsed.general.close_on_action, Some(false));
        assert_eq!(parsed.general.theme.as_deref(), Some("nord"));
        assert_eq!(parsed.addons.emoji.close_on_action, Some(true));
        assert_eq!(parsed.addons.script_filters[0].close_on_action, Some(false));
        assert_eq!(parsed.addons.web_searches[0].close_on_action, Some(true));
        assert_eq!(parsed.entries[0].close_on_action, Some(true));
    }

    #[test]
    fn test_cli_override_rejects_invalid_keys_and_values() {
        let yaml_config = "version: 1\nlaunchers:\n  shell:\n    binary: sh\n";
        let bad_key = Args {
            options: vec!["general.nonexistent=true".into()],
            ..Args::default()
        };
        let err = read_config_from_reader(Cursor::new(yaml_config), &bad_key).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("nonexistent"), "{msg}");

        let bad_launcher_key = Args {
            options: vec!["launchers.shell.nonexistent=true".into()],
            ..Args::default()
        };
        let err = read_config_from_reader(Cursor::new(yaml_config), &bad_launcher_key).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("nonexistent"), "{msg}");

        let bad_type = Args {
            options: vec!["general.close_on_action=not-a-bool".into()],
            ..Args::default()
        };
        let err = read_config_from_reader(Cursor::new(yaml_config), &bad_type).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("close_on_action") || msg.contains("not-a-bool"),
            "{msg}"
        );

        let missing_eq = Args {
            options: vec!["general.close_on_action".into()],
            ..Args::default()
        };
        assert!(read_config_from_reader(Cursor::new(yaml_config), &missing_eq).is_err());

        for bad_spec in [
            "addons.emoji={nonexistent: true}",
            "launchers.shell=42",
            "launchers.shell={binary: sh, nonexistent: true}",
            "addons.devtools.password_length=1",
        ] {
            let bad = Args {
                options: vec![bad_spec.into()],
                ..Args::default()
            };
            assert!(
                read_config_from_reader(Cursor::new(yaml_config), &bad).is_err(),
                "expected error for {bad_spec}"
            );
        }
    }
}
