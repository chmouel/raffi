mod actions;
mod ansi;
mod app;
mod audio;
mod browser;
mod clipboard;
mod clipboard_history;
mod clipboard_mode;
mod color;
mod command_list;
mod currency;
mod devtools;
mod dnd;
mod emoji;
mod help;
mod hyprland;
mod kill;
mod media;
mod placeholders;
mod preview;
mod script_filters;
mod snippets;
mod state;
mod support;
mod system;
mod theme;
mod types;
mod units;
mod view;
mod windows;
mod world_clock;

#[cfg(test)]
mod tests;

use anyhow::Result;
use iced::window;
use std::sync::{Arc, Mutex};

use super::{UISettings, UI};
use crate::{AddonsConfig, RaffiConfig};

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

fn configure_preview_visibility(view: &mut state::ViewState, show_preview: Option<bool>) {
    view.show_preview = true;
    view.show_launcher_preview = show_preview == Some(true);
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
    let iced_theme = if settings.theme.is_light() {
        iced::Theme::Light
    } else {
        iced::Theme::Dark
    };
    let selected_item: SharedSelection = Arc::new(Mutex::new(None));
    let selected_item_clone = selected_item.clone();
    let mut addons = addons.clone();
    let mut hyprland =
        hyprland::default_to_hypr_paste(&mut addons.text_snippets, hyprland::in_session(), || {
            hyprland::Hyprland::capture(&settings.terminal_classes)
        });
    if hyprland.is_none() && addons.clipboard.uses_hypr_paste() && hyprland::in_session() {
        hyprland = Some(hyprland::Hyprland::capture(&settings.terminal_classes));
    }
    let deferred_action = Arc::new(Mutex::new(None::<actions::DeferredAction>));
    let deferred_action_for_new = deferred_action.clone();

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
    let clipboard_history = settings.clipboard_history;
    let show_preview = settings.show_preview;
    let preview_style = theme::PreviewStyle::resolve(
        &theme_colors,
        &settings.preview,
        settings.font_family.as_deref(),
    );
    let default_script_shell = settings.default_script_shell.clone();

    let window_settings = window::Settings {
        size: iced::Size::new(window_width, window_height),
        position: window::Position::Centered,
        decorations: false,
        transparent: true,
        visible: true,
        level: window::Level::AlwaysOnTop,
        #[cfg(target_os = "linux")]
        platform_specific: window::settings::PlatformSpecific {
            application_id: settings
                .app_name
                .as_deref()
                .unwrap_or("com.chmouel.raffi")
                .to_string(),
            ..Default::default()
        },
        #[cfg(not(target_os = "linux"))]
        platform_specific: Default::default(),
        ..Default::default()
    };

    let mut app = iced::application(
        move || {
            let (mut app, task) = LauncherApp::new(
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
            );
            app.text_snippets.clipboard_history = clipboard_history;
            configure_preview_visibility(&mut app.view, show_preview);
            app.view.preview_style = preview_style.clone();
            app.view.window_size = iced::Size::new(window_width, window_height);
            app.view.default_script_shell = default_script_shell.clone();
            app.hyprland = hyprland.clone();
            app.deferred_action = deferred_action_for_new.clone();
            (app, task)
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

    actions::run_deferred(&deferred_action).map_err(|error| anyhow::anyhow!(error))?;

    if let Ok(selected) = selected_item.lock() {
        if let Some(item) = selected.clone() {
            return Ok(item);
        }
    }

    Ok(String::new())
}
