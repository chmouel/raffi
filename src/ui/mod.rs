use crate::{RaffiConfig, UIType};
use anyhow::Result;

pub mod fuzzel;
pub mod native;
pub mod wayland;

/// Trait for UI implementations
pub trait UI {
    /// Show the UI with the given configurations and return the selected item description
    fn show(&self, configs: &[RaffiConfig], no_icons: bool) -> Result<String>;
}

/// Get the appropriate UI implementation based on the UI type
pub fn get_ui(ui_type: UIType) -> Box<dyn UI> {
    match ui_type {
        UIType::Fuzzel => Box::new(fuzzel::FuzzelUI),
        UIType::Native => Box::new(native::NativeUI),
        UIType::Wayland => Box::new(wayland::WaylandUI),
    }
}
