mod actions;
mod ansi;
mod app;
mod browser;
mod currency;
mod emoji;
mod script_filters;
mod snippets;
mod state;
mod support;
mod theme;
mod types;
mod view;

#[cfg(test)]
mod tests;

use anyhow::Result;
use iced::window;
use std::sync::{Arc, Mutex};

use super::{UISettings, UI};
use crate::{AddonsConfig, RaffiConfig, ThemeMode};

use self::state::{LauncherApp, SharedSelection};
use self::theme::ThemeColors;

/// Wayland UI implementation using iced
pub struct WaylandUI;

impl UI for WaylandUI {
    fn show(
        &self,
        configs: &[RaffiConfig],
        addons: &AddonsConfig,
        settings: &UISettings,
    ) -> Result<String> {
        run_wayland_ui(configs, addons, settings)
    }
}

/// Run the Wayland UI with the provided configurations and return the selected item.
fn run_wayland_ui(
    configs: &[RaffiConfig],
    addons: &AddonsConfig,
    settings: &UISettings,
) -> Result<String> {
    crate::debug_log!(
        "wayland: starting UI: {}x{} theme={:?} sort_mode={:?} no_icons={} max_history={}",
        settings.window_width,
        settings.window_height,
        settings.theme,
        settings.sort_mode,
        settings.no_icons,
        settings.max_history
    );
    let theme_colors =
        ThemeColors::from_mode_with_overrides(&settings.theme, settings.theme_colors.as_ref());
    let iced_theme = match settings.theme {
        ThemeMode::Dark => iced::Theme::Dark,
        ThemeMode::Light => iced::Theme::Light,
    };
    let selected_item: SharedSelection = Arc::new(Mutex::new(None));
    let selected_item_clone = selected_item.clone();

    let configs_owned = configs.to_vec();
    let addons_owned = addons.clone();

    let configs_for_new = configs_owned.clone();
    let addons_for_new = addons_owned.clone();
    let selected_item_for_new = selected_item_clone.clone();
    let initial_query_owned = settings.initial_query.clone();
    let no_icons = settings.no_icons;
    let max_history = settings.max_history;
    let font_sizes = settings.font_sizes;
    let window_width = settings.window_width;
    let window_height = settings.window_height;
    let sort_mode = settings.sort_mode.clone();
    let fallbacks = settings.fallbacks.clone();

    let window_settings = window::Settings {
        size: iced::Size::new(window_width, window_height),
        position: window::Position::Centered,
        decorations: false,
        transparent: true,
        visible: true,
        level: window::Level::AlwaysOnTop,
        #[cfg(target_os = "linux")]
        platform_specific: window::settings::PlatformSpecific {
            application_id: "com.chmouel.raffi".to_string(),
            ..Default::default()
        },
        #[cfg(not(target_os = "linux"))]
        platform_specific: Default::default(),
        ..Default::default()
    };

    let mut app = iced::application(
        move || {
            LauncherApp::new(
                configs_for_new.clone(),
                addons_for_new.clone(),
                no_icons,
                selected_item_for_new.clone(),
                initial_query_owned.clone(),
                theme_colors,
                max_history,
                font_sizes,
                sort_mode.clone(),
                fallbacks.clone(),
            )
        },
        LauncherApp::update,
        LauncherApp::view,
    )
    .subscription(LauncherApp::subscription)
    .theme(move |_state: &LauncherApp| iced_theme.clone())
    .window(window_settings);

    if let Some(ref family) = settings.font_family {
        let family_owned = family.clone();
        let family_static: &'static str = Box::leak(family_owned.into_boxed_str());
        app = app.default_font(iced::Font {
            family: iced::font::Family::Name(family_static),
            ..iced::Font::default()
        });
    }

    let result = app.run();
    if let Err(error) = result {
        return Err(anyhow::anyhow!("Failed to run UI: {:?}", error));
    }

    if let Ok(selected) = selected_item.lock() {
        if let Some(item) = selected.clone() {
            return Ok(item);
        }
    }

    Ok(String::new())
}
