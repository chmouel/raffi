use std::path::PathBuf;

use anyhow::Result;

use crate::{AddonsConfig, RaffiConfig, SortMode, ThemeColorsConfig, ThemeMode, UIType};

mod fuzzel;
#[cfg(feature = "wayland")]
mod wayland;

use self::fuzzel::FuzzelUI;
#[cfg(feature = "wayland")]
use self::wayland::WaylandUI;

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
    pub theme: ThemeMode,
    pub theme_colors: Option<ThemeColorsConfig>,
    pub max_history: u32,
    pub font_sizes: FontSizes,
    pub font_family: Option<String>,
    pub window_width: f32,
    pub window_height: f32,
    pub sort_mode: SortMode,
}

impl Default for UISettings {
    fn default() -> Self {
        Self {
            no_icons: false,
            initial_query: None,
            theme: ThemeMode::Dark,
            theme_colors: None,
            max_history: 10,
            font_sizes: FontSizes::default_sizes(),
            font_family: None,
            window_width: 800.0,
            window_height: 600.0,
            sort_mode: SortMode::default(),
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
    let cache_dir = std::env::var("XDG_CACHE_HOME")
        .unwrap_or_else(|_| format!("{}/.cache", std::env::var("HOME").unwrap_or_default()));
    let mut path = PathBuf::from(cache_dir);
    path.push("raffi");
    std::fs::create_dir_all(&path)?;
    path.push("mru.cache");
    Ok(path)
}

/// Get the command history cache file path
pub fn get_history_cache_path() -> Result<PathBuf> {
    let cache_dir = std::env::var("XDG_CACHE_HOME")
        .unwrap_or_else(|_| format!("{}/.cache", std::env::var("HOME").unwrap_or_default()));
    let mut path = PathBuf::from(cache_dir);
    path.push("raffi");
    std::fs::create_dir_all(&path)?;
    path.push("history.cache");
    Ok(path)
}
