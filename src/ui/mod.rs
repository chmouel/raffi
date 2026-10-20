use std::path::PathBuf;

use anyhow::Result;

use crate::{
    AddonsConfig, ClipboardHistory, PreviewConfig, RaffiConfig, Renderer, SortMode,
    ThemeColorsConfig, ThemeMode, UIType,
};

mod fuzzel;
#[cfg(feature = "wayland")]
mod wayland;

use self::fuzzel::FuzzelUI;
#[cfg(feature = "wayland")]
pub(crate) use self::wayland::clear_slack_cache;
#[cfg(feature = "wayland")]
use self::wayland::WaylandUI;
#[cfg(feature = "wayland")]
pub(crate) use self::wayland::THUMBNAIL_DIR_NAME;

/// Font and padding sizes derived from the base font_size.
/// All sizes scale proportionally from the base.
#[derive(Debug, Clone, Copy)]
pub struct FontSizes {
    /// Search input font size (default: 24.0)
    pub input: f32,
    /// List item title font size (default: 20.0)
    pub item: f32,
    /// Subtitle / secondary text (default: 14.0)
    pub subtitle: f32,
    /// Hint bar text (default: 12.0)
    pub hint: f32,
    /// Search input internal padding (default: 16.0)
    pub input_padding: f32,
    /// List item button padding (default: 12.0)
    pub item_padding: f32,
    /// Outer container margin (default: 20.0)
    pub outer_padding: f32,
    /// Gap above the scrollable list (default: 8.0)
    pub scroll_top_padding: f32,
}

impl FontSizes {
    /// Default sizes (matching the original hardcoded values).
    pub fn default_sizes() -> Self {
        Self {
            input: 24.0,
            item: 20.0,
            subtitle: 14.0,
            hint: 12.0,
            input_padding: 16.0,
            item_padding: 12.0,
            outer_padding: 20.0,
            scroll_top_padding: 8.0,
        }
    }

    /// Derive all sizes from a base font_size.
    /// The base is used as the item size; others scale proportionally.
    pub fn from_base(base: f32) -> Self {
        let ratio = base / 20.0; // 20.0 is the default item size
        Self {
            input: (24.0 * ratio).round(),
            item: base,
            subtitle: (14.0 * ratio).round(),
            hint: (12.0 * ratio).round(),
            input_padding: (16.0 * ratio).round(),
            item_padding: (12.0 * ratio).round(),
            outer_padding: (20.0 * ratio).round(),
            scroll_top_padding: (8.0 * ratio).round(),
        }
    }
}

/// Bundled UI settings to avoid proliferating arguments.
#[derive(Debug, Clone)]
pub struct UISettings {
    pub no_icons: bool,
    pub initial_query: Option<String>,
    pub select_1: bool,
    pub app_name: Option<String>,
    pub theme: ThemeMode,
    pub theme_colors: Option<ThemeColorsConfig>,
    pub renderer: Renderer,
    pub max_history: u32,
    pub font_sizes: FontSizes,
    pub font_family: Option<String>,
    pub window_width: f32,
    pub window_height: f32,
    pub sort_mode: SortMode,
    pub fallbacks: Vec<String>,
    pub main_view: Option<Vec<String>>,
    pub clipboard_history: ClipboardHistory,
    pub terminal_classes: Vec<String>,
    pub show_preview: Option<bool>,
    pub show_tips: bool,
    pub preview: PreviewConfig,
    pub default_script_shell: String,
}

impl Default for UISettings {
    fn default() -> Self {
        Self {
            no_icons: false,
            initial_query: None,
            select_1: false,
            app_name: None,
            theme: ThemeMode::Dark,
            theme_colors: None,
            renderer: Renderer::default(),
            max_history: 10,
            font_sizes: FontSizes::default_sizes(),
            font_family: None,
            window_width: 800.0,
            window_height: 600.0,
            sort_mode: SortMode::default(),
            fallbacks: Vec::new(),
            main_view: None,
            clipboard_history: ClipboardHistory::default(),
            terminal_classes: Vec::new(),
            show_preview: None,
            show_tips: true,
            preview: PreviewConfig::default(),
            default_script_shell: "bash".to_string(),
        }
    }
}

pub trait UI {
    fn show(
        &self,
        configs: &[RaffiConfig],
        addons: &AddonsConfig,
        settings: &UISettings,
    ) -> Result<String>;
}

pub fn get_ui(ui_type: UIType) -> Box<dyn UI> {
    crate::debug_log!("ui: backend selected: {ui_type:?}");
    match ui_type {
        UIType::Fuzzel => Box::new(FuzzelUI),
        #[cfg(feature = "wayland")]
        UIType::Native => Box::new(WaylandUI),
    }
}

/// Get the MRU cache file path
pub fn get_mru_cache_path() -> Result<PathBuf> {
    // Unit tests launch entries and type queries; keep them away from the
    // user's usage and history caches, which derive from this path.
    if cfg!(test) {
        anyhow::bail!("usage caches are disabled in tests");
    }
    let cache_dir = std::env::var("XDG_CACHE_HOME")
        .unwrap_or_else(|_| format!("{}/.cache", std::env::var("HOME").unwrap_or_default()));
    let mut path = PathBuf::from(cache_dir);
    path.push("raffi");
    std::fs::create_dir_all(&path)?;
    path.push("mru.cache");
    Ok(path)
}

/// Get the text snippet usage cache file path
#[cfg(feature = "wayland")]
pub fn get_snippet_mru_cache_path() -> Result<PathBuf> {
    let mut path = get_mru_cache_path()?;
    path.set_file_name("snippets.cache");
    Ok(path)
}

/// Get the main view recent picks cache file path
#[cfg(feature = "wayland")]
pub fn get_recent_cache_path() -> Result<PathBuf> {
    let mut path = get_mru_cache_path()?;
    path.set_file_name("recent.cache");
    Ok(path)
}

/// Get the password manager usage cache file path
pub fn get_vault_mru_cache_path() -> Result<PathBuf> {
    let mut path = get_mru_cache_path()?;
    path.set_file_name("vault.cache");
    Ok(path)
}

/// Get the Slack addon cache directory: workspace directory, usage and
/// avatars.
#[cfg(feature = "wayland")]
pub fn get_slack_cache_dir() -> Result<PathBuf> {
    let mut path = get_mru_cache_path()?;
    path.set_file_name("slack");
    std::fs::create_dir_all(&path)?;
    Ok(path)
}

/// Get the command history cache file path
pub fn get_history_cache_path() -> Result<PathBuf> {
    let mut path = get_mru_cache_path()?;
    path.set_file_name("history.cache");
    Ok(path)
}
