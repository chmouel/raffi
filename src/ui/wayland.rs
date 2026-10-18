mod actions;
mod ansi;
mod app;
mod audio;
mod bitwarden;
mod bluetooth;
mod browser;
mod browser_tabs;
mod clipboard;
mod clipboard_history;
mod clipboard_mode;
mod color;
mod command_list;
mod currency;
mod devtools;
mod dnd;
mod emoji;
mod favicons;
mod file_preview;
mod help;
mod hyprland;
mod kill;
mod main_view;
mod media;
mod pass;
mod placeholders;
mod preview;
mod recent;
mod script_filters;
mod secret_io;
mod snippets;
mod state;
mod support;
mod system;
mod tab_preview;
mod terminal;
mod theme;
mod thumbnails;
mod types;
mod units;
mod url_match;
mod vault;
mod view;
mod windows;
mod world_clock;

#[cfg(test)]
mod tests;

use anyhow::Result;
use iced::{window, Task};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use super::{UISettings, UI};
use crate::{AddonsConfig, RaffiConfig};

use self::state::{LauncherApp, SharedSelection};
use self::theme::ThemeColors;
pub(crate) use self::thumbnails::DIR_NAME as THUMBNAIL_DIR_NAME;

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

/// Set when Raffi exported `ICED_BACKEND` itself and must take it back.
static RENDERER_OVERRIDE: AtomicBool = AtomicBool::new(false);

/// iced reads its renderer only from `ICED_BACKEND`. Must run before any
/// thread starts, since setting the environment is not thread safe.
fn select_renderer(renderer: crate::Renderer) {
    if std::env::var_os("ICED_BACKEND").is_some() {
        return;
    }
    if renderer == crate::Renderer::Cpu {
        std::env::set_var("ICED_BACKEND", "tiny-skia");
        RENDERER_OVERRIDE.store(true, Ordering::SeqCst);
    }
}

/// Drop Raffi's `ICED_BACKEND` once the window exists, so programs launched
/// from Raffi do not inherit it. Threads are running by then, but Rust's own
/// environment readers and `Command` spawns take the same lock, and Raffi's
/// own background threads (font scan, thumbnails) do not call C `getenv`.
pub(super) fn release_renderer_override() {
    if RENDERER_OVERRIDE.swap(false, Ordering::SeqCst) {
        std::env::remove_var("ICED_BACKEND");
    }
}

/// Load the system fonts in the background: iced scans every installed font
/// on first use, which otherwise delays the window behind GPU and config setup.
fn prewarm_fonts() {
    std::thread::spawn(|| {
        let _ = iced::advanced::graphics::text::font_system();
        crate::debug_log!("startup: fonts loaded at {}ms", crate::debug::elapsed_ms());
    });
}

/// Run the Wayland UI with the provided configurations and return the selected item.
fn run_wayland_ui(
    configs: &[RaffiConfig],
    addons: &AddonsConfig,
    settings: &UISettings,
) -> Result<String> {
    select_renderer(settings.renderer);
    prewarm_fonts();
    crate::debug_log!(
        "wayland: starting UI: {}x{} theme={:?} sort_mode={:?} no_icons={} max_history={} renderer={:?}",
        settings.window_width,
        settings.window_height,
        settings.theme,
        settings.sort_mode,
        settings.no_icons,
        settings.max_history,
        settings.renderer
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
    let main_view = settings.main_view.clone();
    let clipboard_history = settings.clipboard_history;
    let show_preview = settings.show_preview;
    let select_1 = settings.select_1;
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
            if select_1 && !app.search_query.is_empty() {
                app.select_1_query = Some(app.search_query.clone());
            }
            app.text_snippets.clipboard_history = clipboard_history;
            configure_preview_visibility(&mut app.view, show_preview);
            app.view.preview_style = preview_style.clone();
            app.view.window_size = iced::Size::new(window_width, window_height);
            app.view.default_script_shell = default_script_shell.clone();
            app.file_browser.preview_command = file_preview::FilePreviewCommand::resolve(
                app.addons.file_browser.preview_command.as_deref(),
                app.program_exists,
            );
            app.hyprland = hyprland.clone();
            app.deferred_action = deferred_action_for_new.clone();
            let preload = app.configure_main_view(main_view.as_deref());
            (app, Task::batch([task, preload]))
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
    release_renderer_override();

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
