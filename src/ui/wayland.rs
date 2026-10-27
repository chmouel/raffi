mod action_input;
mod action_menu;
mod actions;
mod ansi;
mod app;
mod audio;
mod bitwarden;
mod bluetooth;
mod bookmarks;
mod bookmarks_mode;
mod browser;
mod browser_data;
mod browser_history;
mod browser_history_mode;
mod browser_tabs;
mod cache;
mod clipboard;
mod clipboard_history;
mod clipboard_mode;
mod color;
mod command_list;
mod currency;
mod devtools;
mod dnd;
mod emoji;
mod empty_states;
mod favicons;
mod file_preview;
mod help;
mod hot_files;
mod hyprland;
mod kill;
mod main_view;
mod media;
mod onboarding;
mod pass;
mod placeholders;
mod preview;
mod ranking;
mod recent;
mod script_filters;
mod secret_io;
mod server_refresh;
mod slack;
mod slack_api;
mod slack_emoji;
mod slack_mode;
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
mod window_events;
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

pub(crate) use self::slack_api::clear_cache as clear_slack_cache;
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

/// Mesa driconf option, also read from the environment. iced takes the first
/// non-sRGB surface format, which Mesa lists as a 10-bit one with a 2-bit
/// alpha channel: opacity then snaps to 0, 1/3, 2/3 or 1. This puts 8-bit
/// BGRA first instead. Other Vulkan drivers ignore it.
const MESA_BGRA8_FIRST: &str = "vk_wsi_force_bgra8_unorm_first";

/// Set when Raffi exported `MESA_BGRA8_FIRST` itself and must take it back.
static FORMAT_OVERRIDE: AtomicBool = AtomicBool::new(false);

/// Set when Raffi exported `MESA_DEBUG=silent`, which stops Mesa printing an
/// "overridden by environment" notice for `MESA_BGRA8_FIRST` on stderr.
static MESA_SILENT_OVERRIDE: AtomicBool = AtomicBool::new(false);

/// iced reads its renderer only from `ICED_BACKEND`. Must run before any
/// thread starts, since setting the environment is not thread safe.
/// Returns whether the GPU renderer is used: the software one presents
/// opaque buffers on Wayland, so the window cannot be translucent with it.
fn select_renderer(renderer: crate::Renderer) -> bool {
    let gpu = match std::env::var_os("ICED_BACKEND") {
        Some(backend) => !backend.to_string_lossy().starts_with("tiny-skia"),
        None if renderer == crate::Renderer::Gpu => true,
        None => {
            std::env::set_var("ICED_BACKEND", "tiny-skia");
            RENDERER_OVERRIDE.store(true, Ordering::SeqCst);
            false
        }
    };
    if gpu && std::env::var_os(MESA_BGRA8_FIRST).is_none() {
        std::env::set_var(MESA_BGRA8_FIRST, "true");
        FORMAT_OVERRIDE.store(true, Ordering::SeqCst);
        if std::env::var_os("MESA_DEBUG").is_none() {
            std::env::set_var("MESA_DEBUG", "silent");
            MESA_SILENT_OVERRIDE.store(true, Ordering::SeqCst);
        }
    }
    gpu
}

/// Drop the variables Raffi exported for iced once the window exists, so
/// programs launched from Raffi do not inherit them. Threads are running by
/// then, but Rust's own environment readers and `Command` spawns take the
/// same lock, and Raffi's own background threads (font scan, thumbnails) do
/// not call C `getenv`.
pub(super) fn release_renderer_override() {
    if RENDERER_OVERRIDE.swap(false, Ordering::SeqCst) {
        std::env::remove_var("ICED_BACKEND");
    }
    if FORMAT_OVERRIDE.swap(false, Ordering::SeqCst) {
        std::env::remove_var(MESA_BGRA8_FIRST);
    }
    if MESA_SILENT_OVERRIDE.swap(false, Ordering::SeqCst) {
        std::env::remove_var("MESA_DEBUG");
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

/// One launcher window: its inputs, the per-window state captured before it
/// opens, and the slots its exit paths fill.
struct Launch {
    configs: Vec<RaffiConfig>,
    addons: AddonsConfig,
    settings: UISettings,
    translucent: bool,
    theme_colors: ThemeColors,
    preview_style: theme::PreviewStyle,
    hyprland: Option<Result<hyprland::Hyprland, String>>,
    selected_item: SharedSelection,
    deferred_action: Arc<Mutex<Option<actions::DeferredAction>>>,
}

impl Launch {
    /// Capture what depends on the moment Raffi opens: the clipboard
    /// manager running and, on Hyprland, the focused window.
    fn prepare(
        configs: &[RaffiConfig],
        addons: &AddonsConfig,
        settings: &UISettings,
        translucent: bool,
    ) -> Self {
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
            ThemeColors::from_mode_with_overrides(&settings.theme, settings.theme_colors.as_ref())
                .with_opacity(if translucent { settings.opacity } else { 1.0 });
        let mut addons = addons.clone();
        clipboard_history::resolve_clipboard_addon(
            &mut addons.clipboard,
            clipboard_history::running_cmdlines,
        );
        let mut hyprland = hyprland::default_to_hypr_paste(
            &mut addons.text_snippets,
            hyprland::in_session(),
            || hyprland::Hyprland::capture(&settings.terminal_classes),
        );
        if hyprland.is_none() && addons.clipboard.uses_hypr_paste() && hyprland::in_session() {
            hyprland = Some(hyprland::Hyprland::capture(&settings.terminal_classes));
        }
        let preview_style = theme::PreviewStyle::resolve(
            &theme_colors,
            &settings.preview,
            settings.font_family.as_deref(),
            settings.font_sizes.item,
        );
        Self {
            configs: configs.to_vec(),
            addons,
            settings: settings.clone(),
            translucent,
            theme_colors,
            preview_style,
            hyprland,
            selected_item: Arc::new(Mutex::new(None)),
            deferred_action: Arc::new(Mutex::new(None)),
        }
    }

    fn boot(&self, resident: bool) -> (LauncherApp, Task<types::Message>) {
        let settings = &self.settings;
        let (mut app, task) = LauncherApp::new(
            self.configs.clone(),
            self.addons.clone(),
            settings.no_icons,
            self.selected_item.clone(),
            settings.initial_query.clone(),
            self.theme_colors,
            settings.max_history,
            settings.font_sizes,
            settings.sort_mode.clone(),
            settings.fallbacks.clone(),
            settings.vim_mode,
        );
        if settings.select_1 && !app.search_query.is_empty() {
            app.select_1_query = Some(app.search_query.clone());
        }
        app.initialize_onboarding(
            settings.config_path.clone(),
            settings.existing_configuration,
        );
        app.text_snippets.clipboard_history = settings.clipboard_history;
        app.close_on_action = settings.close_on_action;
        app.print_only = settings.print_only;
        app.resident = resident;
        configure_preview_visibility(&mut app.view, settings.show_preview);
        app.view.preview_style = self.preview_style.clone();
        app.view.window_size = iced::Size::new(settings.window_width, settings.window_height);
        app.view.default_script_shell = settings.default_script_shell.clone();
        app.view.launch_via = settings.launch_via;
        app.view.raise_running = settings.raise_running;
        app.view.raise_via = crate::compositor::Compositor::current();
        app.file_browser.preview_command = file_preview::FilePreviewCommand::resolve(
            app.addons.file_browser.preview_command.as_deref(),
            app.program_exists,
        );
        app.hyprland = self.hyprland.clone();
        app.deferred_action = self.deferred_action.clone();
        app.main_view.chips = settings.main_view_chips;
        let preload = app.configure_main_view(settings.main_view.as_ref());
        if settings.show_tips {
            app.show_search_tip();
        }
        let server = Task::perform(browser_data::off_thread(cache::probe), |info| {
            types::Message::CacheServerStatus(info.flatten())
        });
        crate::debug::mark("app booted");
        (app, Task::batch([task, preload, server]))
    }

    fn iced_theme(&self) -> iced::Theme {
        if self.settings.theme.is_light() {
            iced::Theme::Light
        } else {
            iced::Theme::Dark
        }
    }

    /// iced clears the window with the theme's opaque background; clear it
    /// transparent so the rounded frame and `opacity` show what is behind.
    fn style(&self, theme: &iced::Theme) -> iced::theme::Style {
        let palette = theme.palette();
        iced::theme::Style {
            background_color: if self.translucent {
                iced::Color::TRANSPARENT
            } else {
                palette.background
            },
            text_color: palette.text,
        }
    }

    fn window_settings(&self) -> window::Settings {
        let settings = &self.settings;
        window::Settings {
            size: iced::Size::new(settings.window_width, settings.window_height),
            position: window::Position::Centered,
            decorations: false,
            transparent: true,
            blur: settings.blur,
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
        }
    }

    /// Once the window is gone: run the action queued for after close, then
    /// return the launcher entry picked, empty when none was.
    fn finish(&self) -> Result<String> {
        actions::run_deferred(&self.deferred_action).map_err(|error| anyhow::anyhow!(error))?;
        Ok(self
            .selected_item
            .lock()
            .ok()
            .and_then(|selected| selected.clone())
            .unwrap_or_default())
    }
}

fn default_font(family: &str) -> iced::Font {
    iced::Font {
        family: iced::font::Family::Name(Box::leak(family.to_string().into_boxed_str())),
        ..iced::Font::default()
    }
}

/// Run the Wayland UI with the provided configurations and return the selected item.
fn run_wayland_ui(
    configs: &[RaffiConfig],
    addons: &AddonsConfig,
    settings: &UISettings,
) -> Result<String> {
    let translucent = select_renderer(settings.renderer);
    prewarm_fonts();
    let launch = Arc::new(Launch::prepare(configs, addons, settings, translucent));
    let window_settings = launch.window_settings();
    let (boot, theme, style) = (launch.clone(), launch.clone(), launch.clone());

    let mut app = iced::application(
        move || boot.boot(false),
        LauncherApp::update,
        LauncherApp::view,
    )
    .subscription(LauncherApp::subscription)
    .theme(move |_state: &LauncherApp| theme.iced_theme())
    .style(move |_state: &LauncherApp, iced_theme: &iced::Theme| style.style(iced_theme))
    .window(window_settings);

    if let Some(ref family) = settings.font_family {
        app = app.default_font(default_font(family));
    }

    let result = app.run();
    release_renderer_override();

    if let Err(error) = result {
        return Err(anyhow::anyhow!("Failed to run UI: {:?}", error));
    }

    launch.finish()
}

/// Run the optional background cache service.
pub(crate) fn run_cache_daemon(config: crate::ParsedConfig) -> Result<()> {
    self::cache::run(config)
}

/// Clear every wayland-side disk cache and ask a running daemon to clear its state.
pub(crate) fn clear_wayland_caches() {
    clear_slack_cache();
    self::cache::notify_clear();
    self::cache::clear_snapshots();
    self::hot_files::clear();
}
