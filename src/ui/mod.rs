use anyhow::Result;

use crate::{RaffiConfig, UIType};

mod fuzzel;
mod tui;
mod wayland;

use self::fuzzel::FuzzelUI;
use self::tui::TuiUI;
use self::wayland::WaylandUI;

pub trait UI {
    fn show(&self, configs: &[RaffiConfig], no_icons: bool) -> Result<String>;
}

pub fn get_ui(ui_type: UIType) -> Box<dyn UI> {
    match ui_type {
        UIType::Fuzzel => Box::new(FuzzelUI),
        UIType::Tui => Box::new(TuiUI),
        UIType::Wayland => Box::new(WaylandUI),
    }
}
