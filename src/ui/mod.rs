use std::path::PathBuf;

use anyhow::Result;

use crate::{AddonsConfig, RaffiConfig, UIType};

mod fuzzel;
#[cfg(feature = "wayland")]
mod wayland;

use self::fuzzel::FuzzelUI;
#[cfg(feature = "wayland")]
use self::wayland::WaylandUI;

pub trait UI {
    fn show(
        &self,
        configs: &[RaffiConfig],
        addons: &AddonsConfig,
        no_icons: bool,
        initial_query: Option<&str>,
    ) -> Result<String>;
}

pub fn get_ui(ui_type: UIType) -> Box<dyn UI> {
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
