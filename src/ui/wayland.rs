use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use iced::widget::container::Id as ContainerId;
use iced::widget::scrollable::Id as ScrollableId;
use iced::widget::text_input::Id as TextInputId;
use iced::widget::{
    button, column, container, image, scrollable, svg, text, text_input, Column, Row,
};
use iced::{window, Element, Length, Task};

use super::UI;
use crate::{read_icon_map, RaffiConfig};

const APPLICATION_ID: &str = "com.chmouel.raffi";

/// Wayland UI implementation using iced
pub struct WaylandUI;

impl UI for WaylandUI {
    fn show(&self, configs: &[RaffiConfig], no_icons: bool) -> Result<String> {
        run_wayland_ui(configs, no_icons)
    }
}

/// Shared state for capturing the selected item
type SharedSelection = Arc<Mutex<Option<String>>>;

/// The main application state
struct LauncherApp {
    configs: Vec<RaffiConfig>,
    filtered_configs: Vec<usize>,
    search_query: String,
    selected_index: usize,
    selected_item: SharedSelection,
    icon_map: HashMap<String, String>,
    mru_map: HashMap<String, u32>,
    search_input_id: TextInputId,
    scrollable_id: ScrollableId,
    items_container_id: ContainerId,
    view_generation: u64,
}

#[derive(Debug, Clone)]
enum Message {
    SearchChanged(String),
    MoveUp,
    MoveDown,
    Submit,
    Cancel,
    ItemClicked(usize),
}

impl LauncherApp {
    fn new(
        mut configs: Vec<RaffiConfig>,
        no_icons: bool,
        selected_item: SharedSelection,
    ) -> (Self, Task<Message>) {
        let icon_map = if no_icons {
            HashMap::new()
        } else {
            read_icon_map().unwrap_or_default()
        };

        let mru_map = load_mru_map();
        configs.sort_by_key(|config| {
            let description = config
                .description
                .as_deref()
                .unwrap_or_else(|| config.binary.as_deref().unwrap_or(""));
            -(mru_map.get(description).copied().unwrap_or(0) as i32)
        });

        let filtered_configs: Vec<usize> = (0..configs.len()).collect();
        let search_input_id = TextInputId::unique();
        let scrollable_id = ScrollableId::unique();
        let items_container_id = ContainerId::unique();

        (
            LauncherApp {
                configs,
                filtered_configs,
                search_query: String::new(),
                selected_index: 0,
                selected_item,
                icon_map,
                mru_map,
                search_input_id: search_input_id.clone(),
                scrollable_id,
                items_container_id,
                view_generation: 0,
            },
            text_input::focus(search_input_id),
        )
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::SearchChanged(query) => {
                self.search_query = query.clone();
                self.filter_items(&query);
                self.selected_index = 0;
                // Regenerate IDs to force complete view refresh
                self.scrollable_id = ScrollableId::unique();
                self.items_container_id = ContainerId::unique();
                self.view_generation = self.view_generation.wrapping_add(1);
                Task::none()
            }
            Message::MoveUp => {
                if self.selected_index > 0 {
                    self.selected_index -= 1;
                }
                if self.filtered_configs.len() > 1 {
                    let offset =
                        self.selected_index as f32 / (self.filtered_configs.len() - 1) as f32;
                    scrollable::snap_to(
                        self.scrollable_id.clone(),
                        scrollable::RelativeOffset { x: 0.0, y: offset },
                    )
                } else {
                    Task::none()
                }
            }
            Message::MoveDown => {
                // Move selection down
                if self.selected_index < self.filtered_configs.len().saturating_sub(1) {
                    self.selected_index += 1;
                }
                if self.filtered_configs.len() > 1 {
                    let offset =
                        self.selected_index as f32 / (self.filtered_configs.len() - 1) as f32;
                    scrollable::snap_to(
                        self.scrollable_id.clone(),
                        scrollable::RelativeOffset { x: 0.0, y: offset },
                    )
                } else {
                    Task::none()
                }
            }
            Message::Submit => {
                if !self.filtered_configs.is_empty() {
                    let config_idx = self.filtered_configs[self.selected_index];
                    let config = &self.configs[config_idx];
                    let description = config
                        .description
                        .clone()
                        .unwrap_or_else(|| config.binary.clone().unwrap_or_default());
                    if let Ok(mut selected) = self.selected_item.lock() {
                        *selected = Some(description.clone());
                    }
                    let count = self.mru_map.entry(description).or_insert(0);
                    *count += 1;
                    save_mru_map(&self.mru_map);
                }
                window::get_latest().and_then(window::close)
            }
            Message::Cancel => {
                // Don't set selection, just close
                window::get_latest().and_then(window::close)
            }
            Message::ItemClicked(idx) => {
                // Set the clicked item as selected and submit
                self.selected_index = idx;
                // Execute submit logic
                if !self.filtered_configs.is_empty() && idx < self.filtered_configs.len() {
                    let config_idx = self.filtered_configs[idx];
                    let config = &self.configs[config_idx];
                    let description = config
                        .description
                        .clone()
                        .unwrap_or_else(|| config.binary.clone().unwrap_or_default());
                    if let Ok(mut selected) = self.selected_item.lock() {
                        *selected = Some(description.clone());
                    }
                    let count = self.mru_map.entry(description).or_insert(0);
                    *count += 1;
                    save_mru_map(&self.mru_map);
                }
                window::get_latest().and_then(window::close)
            }
        }
    }

    fn view(&self) -> Element<'_, Message> {
        let search_input = text_input("Type to search...", &self.search_query)
            .id(self.search_input_id.clone())
            .on_input(Message::SearchChanged)
            .on_submit(Message::Submit)
            .padding(15)
            .size(22)
            .style(|_theme, _status| text_input::Style {
                background: iced::Background::Color(iced::Color::from_rgb(0.2, 0.2, 0.25)),
                border: iced::Border {
                    radius: 5.0.into(),
                    width: 1.0,
                    color: iced::Color::from_rgb(0.4, 0.4, 0.5),
                },
                placeholder: iced::Color::from_rgb(0.6, 0.6, 0.7),
                value: iced::Color::WHITE,
                selection: iced::Color::from_rgb(0.4, 0.4, 0.5),
                icon: iced::Color::from_rgb(0.8, 0.8, 0.8),
            })
            .width(Length::Fill);

        let mut items_column = Column::new().spacing(5);

        for (idx, &config_idx) in self.filtered_configs.iter().enumerate() {
            let config = &self.configs[config_idx];
            let description = config
                .description
                .clone()
                .unwrap_or_else(|| config.binary.clone().unwrap_or_default());

            // Get icon path if available
            let mut icon_path = if !self.icon_map.is_empty() {
                let icon_name = config
                    .icon
                    .as_ref()
                    .or(config.binary.as_ref())
                    .cloned()
                    .unwrap_or_default();
                self.icon_map.get(&icon_name).cloned()
            } else {
                None
            };

            if icon_path.is_none() {
                icon_path = Some("assets/default_icon.svg".to_string());
            }

            // Build the row with optional icon
            let mut item_row = Row::new().spacing(15).align_y(iced::Alignment::Center);

            // Add icon if available
            if let Some(icon_path_str) = icon_path {
                let icon_path = PathBuf::from(&icon_path_str);
                if icon_path.exists() {
                    let is_svg = icon_path
                        .extension()
                        .and_then(|ext| ext.to_str())
                        .map(|ext| ext.to_lowercase() == "svg")
                        .unwrap_or(false);

                    if is_svg {
                        let svg_handle = iced::widget::svg::Handle::from_path(&icon_path);
                        item_row = item_row.push(svg(svg_handle).width(48).height(48));
                    } else {
                        item_row = item_row.push(image(icon_path).width(48).height(48));
                    }
                }
            }

            let text_widget = text(description).size(20);
            item_row = item_row.push(text_widget);

            let item_button = button(item_row)
                .on_press(Message::ItemClicked(idx))
                .padding(15)
                .width(Length::Fill);

            let styled_button = if idx == self.selected_index {
                item_button.style(|_theme, _status| button::Style {
                    background: Some(iced::Background::Color(iced::Color::from_rgb(
                        0.4, 0.4, 0.5,
                    ))),
                    border: iced::Border {
                        radius: 5.0.into(),
                        ..Default::default()
                    },
                    text_color: iced::Color::WHITE,
                    ..Default::default()
                })
            } else {
                item_button.style(|_theme, _status| button::Style {
                    background: Some(iced::Background::Color(iced::Color::from_rgb(
                        0.2, 0.2, 0.25,
                    ))),
                    border: iced::Border {
                        radius: 5.0.into(),
                        ..Default::default()
                    },
                    text_color: iced::Color::from_rgb(0.8, 0.8, 0.8),
                    ..Default::default()
                })
            };

            items_column = items_column.push(styled_button);
        }

        let items_container = container(items_column)
            .id(self.items_container_id.clone())
            .width(Length::Fill)
            .height(Length::Shrink);

        let items_scroll = scrollable(items_container)
            .id(self.scrollable_id.clone())
            .height(Length::Fill)
            .width(Length::Fill);

        let content = column![search_input, items_scroll]
            .spacing(10)
            .width(Length::Fill)
            .height(Length::Fill);

        container(content)
            .padding(10)
            .width(Length::Fill)
            .height(Length::Fill)
            .clip(true)
            .style(|_theme| container::Style {
                background: Some(iced::Background::Color(iced::Color::from_rgb(
                    0.1, 0.1, 0.15,
                ))),
                ..Default::default()
            })
            .into()
    }

    fn subscription(&self) -> iced::Subscription<Message> {
        use iced::keyboard;
        use iced::keyboard::key::Named;
        use iced::{event, Event};

        event::listen_with(|event, _status, _id| match event {
            Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(Named::ArrowDown),
                ..
            }) => Some(Message::MoveDown),
            Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(Named::ArrowUp),
                ..
            }) => Some(Message::MoveUp),
            Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(Named::Enter),
                ..
            }) => Some(Message::Submit),
            Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(Named::Escape),
                ..
            }) => Some(Message::Cancel),
            _ => None,
        })
    }

    fn filter_items(&mut self, query: &str) {
        if query.is_empty() {
            self.filtered_configs = (0..self.configs.len()).collect();
        } else {
            let query_lower = query.to_lowercase();
            self.filtered_configs = self
                .configs
                .iter()
                .enumerate()
                .filter(|(_, config)| {
                    let description = config
                        .description
                        .as_ref()
                        .or(config.binary.as_ref())
                        .map(|s| s.to_lowercase())
                        .unwrap_or_default();
                    description.contains(&query_lower)
                })
                .map(|(idx, _)| idx)
                .collect();
        }
    }
}

fn get_mru_file_path() -> Result<PathBuf> {
    let cache_dir = std::env::var("XDG_CACHE_HOME")
        .unwrap_or_else(|_| format!("{}/.cache", std::env::var("HOME").unwrap_or_default()));
    let mut path = PathBuf::from(cache_dir);
    path.push("raffi");
    fs::create_dir_all(&path)?;
    path.push("mru.cache");
    Ok(path)
}

fn load_mru_map() -> HashMap<String, u32> {
    if let Ok(path) = get_mru_file_path() {
        if let Ok(content) = fs::read_to_string(path) {
            let mut map = HashMap::new();
            for line in content.lines() {
                let mut parts = line.splitn(2, '|');
                if let (Some(desc), Some(count_str)) = (parts.next(), parts.next()) {
                    if let Ok(count) = count_str.parse::<u32>() {
                        map.insert(desc.to_string(), count);
                    }
                }
            }
            return map;
        }
    }
    HashMap::new()
}

fn save_mru_map(map: &HashMap<String, u32>) {
    if let Ok(path) = get_mru_file_path() {
        let mut entries: Vec<_> = map.iter().collect();
        entries.sort_by(|a, b| b.1.cmp(a.1));
        let content = entries
            .iter()
            .map(|(desc, count)| format!("{}|{}", desc, count))
            .collect::<Vec<_>>()
            .join("\n");
        let _ = fs::write(path, content);
    }
}

/// Run the Wayland UI with the provided configurations and return the selected item.
fn run_wayland_ui(configs: &[RaffiConfig], no_icons: bool) -> Result<String> {
    let selected_item: SharedSelection = Arc::new(Mutex::new(None));
    let selected_item_clone = selected_item.clone();

    // Clone configs to own them for the 'static lifetime requirement
    let configs_owned = configs.to_vec();

    let result = iced::application("Raffi Launcher", LauncherApp::update, LauncherApp::view)
        .subscription(LauncherApp::subscription)
        .theme(|_state: &LauncherApp| iced::Theme::Dark)
        .window(window::Settings {
            size: iced::Size::new(800.0, 600.0),
            position: window::Position::Centered,
            decorations: false,
            transparent: true,
            visible: true,
            level: window::Level::AlwaysOnTop,
            platform_specific: iced::window::settings::PlatformSpecific {
                application_id: APPLICATION_ID.to_string(),
                ..Default::default()
            },
            ..Default::default()
        })
        .run_with(move || LauncherApp::new(configs_owned, no_icons, selected_item_clone));

    if let Err(e) = result {
        return Err(anyhow::anyhow!("Failed to run UI: {:?}", e));
    }

    // Retrieve the selected item from the shared state
    if let Ok(selected) = selected_item.lock() {
        if let Some(item) = selected.clone() {
            return Ok(item);
        }
    }

    Ok(String::new())
}
