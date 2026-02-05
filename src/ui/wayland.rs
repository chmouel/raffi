use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;
use iced::widget::operation::{focus, snap_to};
use iced::widget::{
    button, column, container, image, scrollable, svg, text, text_input, Column, Id, Row,
};
use iced::window;
use iced::{Element, Length, Task};

type ContainerId = Id;
type ScrollableId = Id;
type TextInputId = Id;

use super::UI;
use crate::{read_icon_map, RaffiConfig};

// --- Theme Colors (Dracula-ish with transparency) ---
const COLOR_BG_BASE: iced::Color = iced::Color {
    r: 0.15,
    g: 0.16,
    b: 0.21,
    a: 0.95,
}; // Dark Blue-Grey
const COLOR_BG_INPUT: iced::Color = iced::Color {
    r: 0.26,
    g: 0.27,
    b: 0.35,
    a: 1.0,
}; // Lighter Blue-Grey
const COLOR_ACCENT: iced::Color = iced::Color {
    r: 0.74,
    g: 0.57,
    b: 0.97,
    a: 1.0,
}; // Purple
const COLOR_ACCENT_HOVER: iced::Color = iced::Color {
    r: 0.54,
    g: 0.91,
    b: 0.99,
    a: 1.0,
}; // Cyan
const COLOR_TEXT_MAIN: iced::Color = iced::Color::WHITE;
const COLOR_TEXT_MUTED: iced::Color = iced::Color {
    r: 0.38,
    g: 0.44,
    b: 0.64,
    a: 1.0,
}; // Blueish Grey
const COLOR_SELECTION_BG: iced::Color = iced::Color {
    r: 0.27,
    g: 0.29,
    b: 0.36,
    a: 0.8,
}; // Selection HL
const COLOR_BORDER: iced::Color = iced::Color {
    r: 0.38,
    g: 0.44,
    b: 0.64,
    a: 0.5,
};

/// Wayland UI implementation using iced
pub struct WaylandUI;

impl UI for WaylandUI {
    fn show(&self, configs: &[RaffiConfig], no_icons: bool) -> Result<String> {
        run_wayland_ui(configs, no_icons)
    }
}

/// Shared state for capturing the selected item
type SharedSelection = Arc<Mutex<Option<String>>>;

/// Calculator result for math expression evaluation
#[derive(Debug, Clone)]
struct CalculatorResult {
    expression: String,
    result: f64,
}

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
    calculator_result: Option<CalculatorResult>,
}

#[derive(Debug, Clone)]
enum Message {
    SearchChanged(String),
    MoveUp,
    MoveDown,
    Submit,
    Cancel,
    ItemClicked(usize),
    CalculatorSelected,
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
            std::cmp::Reverse(mru_map.get(description).copied().unwrap_or(0))
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
                calculator_result: None,
            },
            focus(search_input_id),
        )
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::SearchChanged(query) => {
                self.search_query = query.clone();
                self.filter_items(&query);
                self.calculator_result = try_evaluate_math(&query);
                self.selected_index = 0;
                // Regenerate IDs to force complete view refresh
                self.scrollable_id = ScrollableId::unique();
                self.items_container_id = ContainerId::unique();
                self.view_generation = self.view_generation.wrapping_add(1);
                Task::none()
            }
            Message::MoveUp => {
                let total = self.total_items();
                if self.selected_index > 0 {
                    self.selected_index -= 1;
                } else if total > 0 {
                    // Wrap around to bottom
                    self.selected_index = total - 1;
                }

                if total > 1 {
                    let offset = self.selected_index as f32 / (total - 1) as f32;
                    snap_to(
                        self.scrollable_id.clone(),
                        scrollable::RelativeOffset { x: 0.0, y: offset },
                    )
                } else {
                    Task::none()
                }
            }
            Message::MoveDown => {
                let total = self.total_items();
                // Move selection down
                if self.selected_index < total.saturating_sub(1) {
                    self.selected_index += 1;
                } else {
                    // Wrap around to top
                    self.selected_index = 0;
                }

                if total > 1 {
                    let offset = self.selected_index as f32 / (total - 1) as f32;
                    snap_to(
                        self.scrollable_id.clone(),
                        scrollable::RelativeOffset { x: 0.0, y: offset },
                    )
                } else {
                    Task::none()
                }
            }
            Message::Submit => {
                // Check if calculator is selected (index 0 when calculator is shown)
                if self.calculator_result.is_some() && self.selected_index == 0 {
                    return self.update(Message::CalculatorSelected);
                }

                // Adjust index for config lookup when calculator is shown
                let config_index = if self.calculator_result.is_some() {
                    self.selected_index.saturating_sub(1)
                } else {
                    self.selected_index
                };

                if let Some(&config_idx) = self.filtered_configs.get(config_index) {
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
                iced::exit()
            }
            Message::Cancel => {
                // Don't set selection, just close
                iced::exit()
            }
            Message::ItemClicked(idx) => {
                // Set the clicked item as selected and submit
                self.selected_index = idx;

                // Check if calculator is clicked (index 0 when calculator is shown)
                if self.calculator_result.is_some() && idx == 0 {
                    return self.update(Message::CalculatorSelected);
                }

                // Adjust index for config lookup when calculator is shown
                let config_index = if self.calculator_result.is_some() {
                    idx.saturating_sub(1)
                } else {
                    idx
                };

                if let Some(&config_idx) = self.filtered_configs.get(config_index) {
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
                iced::exit()
            }
            Message::CalculatorSelected => {
                if let Some(ref calc) = self.calculator_result {
                    // Format the result nicely (remove trailing zeros for whole numbers)
                    let result_str = if calc.result.fract() == 0.0 {
                        format!("{}", calc.result as i64)
                    } else {
                        format!("{}", calc.result)
                    };
                    // Copy result to clipboard using wl-copy
                    let _ = Command::new("wl-copy")
                        .arg(&result_str)
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .spawn();
                }
                iced::exit()
            }
        }
    }

    fn view(&self) -> Element<'_, Message> {
        // --- Search Input Styling ---
        let search_input = text_input("Type to search...", &self.search_query)
            .id(self.search_input_id.clone())
            .on_input(Message::SearchChanged)
            .on_submit(Message::Submit)
            .padding(16)
            .size(24)
            .style(|_theme, status| {
                let border_color = if matches!(status, text_input::Status::Focused { .. }) {
                    COLOR_ACCENT
                } else {
                    COLOR_BORDER
                };

                text_input::Style {
                    background: iced::Background::Color(COLOR_BG_INPUT),
                    border: iced::Border {
                        radius: 12.0.into(),
                        width: 1.0,
                        color: border_color,
                    },
                    placeholder: COLOR_TEXT_MUTED,
                    value: COLOR_TEXT_MAIN,
                    selection: COLOR_ACCENT,
                    icon: COLOR_TEXT_MUTED,
                }
            })
            .width(Length::Fill);

        // --- List Items ---
        let mut items_column = Column::new().spacing(6);

        // Track if calculator result is shown to offset config indices
        let has_calculator = self.calculator_result.is_some();

        // Add calculator result as first item if present
        if let Some(ref calc) = self.calculator_result {
            let result_str = if calc.result.fract() == 0.0 {
                format!("{}", calc.result as i64)
            } else {
                format!("{}", calc.result)
            };

            let calc_text = format!("= {} = {}", calc.expression, result_str);
            let calc_row = Row::new()
                .spacing(16)
                .align_y(iced::Alignment::Center)
                .push(text(calc_text).size(20).color(COLOR_ACCENT));

            let is_selected = self.selected_index == 0;

            let calc_button = button(calc_row)
                .on_press(Message::CalculatorSelected)
                .padding(12)
                .width(Length::Fill)
                .style(move |_theme, status| {
                    let base_style = button::Style {
                        text_color: COLOR_ACCENT,
                        border: iced::Border {
                            radius: 8.0.into(),
                            ..Default::default()
                        },
                        ..Default::default()
                    };

                    if is_selected {
                        button::Style {
                            background: Some(iced::Background::Color(COLOR_SELECTION_BG)),
                            border: iced::Border {
                                color: COLOR_ACCENT,
                                width: 1.0,
                                radius: 8.0.into(),
                            },
                            ..base_style
                        }
                    } else {
                        match status {
                            button::Status::Hovered => button::Style {
                                background: Some(iced::Background::Color(iced::Color {
                                    a: 0.1,
                                    ..COLOR_ACCENT_HOVER
                                })),
                                ..base_style
                            },
                            _ => button::Style {
                                background: None,
                                ..base_style
                            },
                        }
                    }
                });

            items_column = items_column.push(calc_button);
        }

        if self.filtered_configs.is_empty() && !has_calculator {
            let no_results = container(
                text("No matching results found.")
                    .size(18)
                    .color(COLOR_TEXT_MUTED),
            )
            .width(Length::Fill)
            .height(Length::Fill)
            .align_x(iced::Alignment::Center)
            .align_y(iced::Alignment::Center);

            // If no results, we just show the message in the scroll area
            items_column = items_column.push(no_results);
        } else {
            for (idx, &config_idx) in self.filtered_configs.iter().enumerate() {
                // Adjust display index when calculator is shown
                let display_idx = if has_calculator { idx + 1 } else { idx };
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
                    let default_path = "assets/default_icon.svg";
                    if Path::new(default_path).exists() {
                        icon_path = Some(default_path.to_string());
                    }
                }

                // Build the row with optional icon
                let mut item_row = Row::new().spacing(16).align_y(iced::Alignment::Center);

                // Add icon if available
                if let Some(icon_path_str) = icon_path {
                    let icon_path = PathBuf::from(&icon_path_str);
                    if icon_path.exists() {
                        let is_svg = icon_path
                            .extension()
                            .and_then(|ext| ext.to_str())
                            .map(|ext| ext.to_lowercase() == "svg")
                            .unwrap_or(false);

                        // Icon Container for consistent sizing and alignment
                        let icon_content: Element<Message> = if is_svg {
                            svg(iced::widget::svg::Handle::from_path(&icon_path))
                                .width(Length::Fixed(40.0))
                                .height(Length::Fixed(40.0))
                                .content_fit(iced::ContentFit::Contain)
                                .into()
                        } else {
                            image(icon_path)
                                .width(Length::Fixed(40.0))
                                .height(Length::Fixed(40.0))
                                .content_fit(iced::ContentFit::Contain)
                                .into()
                        };

                        item_row = item_row.push(icon_content);
                    }
                }

                // Text Content
                let text_widget = text(description).size(20).width(Length::Fill);
                item_row = item_row.push(text_widget);

                let is_selected = display_idx == self.selected_index;

                let item_button = button(item_row)
                    .on_press(Message::ItemClicked(display_idx))
                    .padding(12)
                    .width(Length::Fill)
                    .style(move |_theme, status| {
                        let base_style = button::Style {
                            text_color: COLOR_TEXT_MAIN,
                            border: iced::Border {
                                radius: 8.0.into(),
                                ..Default::default()
                            },
                            ..Default::default()
                        };

                        if is_selected {
                            button::Style {
                                background: Some(iced::Background::Color(COLOR_SELECTION_BG)),
                                border: iced::Border {
                                    color: COLOR_ACCENT,
                                    width: 1.0,
                                    radius: 8.0.into(),
                                },
                                ..base_style
                            }
                        } else {
                            match status {
                                button::Status::Hovered => button::Style {
                                    background: Some(iced::Background::Color(iced::Color {
                                        a: 0.1,
                                        ..COLOR_ACCENT_HOVER
                                    })),
                                    ..base_style
                                },
                                _ => button::Style {
                                    background: None, // Transparent by default
                                    ..base_style
                                },
                            }
                        }
                    });

                items_column = items_column.push(item_button);
            }
        }

        let items_container = container(items_column)
            .id(self.items_container_id.clone())
            .width(Length::Fill)
            .height(Length::Shrink);

        let items_scroll = scrollable(items_container)
            .id(self.scrollable_id.clone())
            .height(Length::Fill)
            .width(Length::Fill);

        // Main Layout
        let content = column![
            search_input,
            container(items_scroll).padding(iced::Padding {
                top: 8.0,
                right: 4.0,
                bottom: 0.0,
                left: 0.0
            })
        ]
        .spacing(12)
        .width(Length::Fill)
        .height(Length::Fill);

        container(content)
            .padding(20)
            .width(Length::Fill)
            .height(Length::Fill)
            .style(|_theme| container::Style {
                background: Some(iced::Background::Color(COLOR_BG_BASE)),
                border: iced::Border {
                    color: COLOR_BORDER,
                    width: 1.0,
                    radius: 16.0.into(),
                },
                text_color: Some(COLOR_TEXT_MAIN),
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
            self.filtered_configs = fuzzy_match_configs(&self.configs, query);
        }
    }

    fn total_items(&self) -> usize {
        let calc_offset = if self.calculator_result.is_some() {
            1
        } else {
            0
        };
        self.filtered_configs.len() + calc_offset
    }
}

fn try_evaluate_math(query: &str) -> Option<CalculatorResult> {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return None;
    }

    // Check if expression looks like math: must contain an operator
    let has_operator = trimmed.contains('+')
        || trimmed.contains('-')
        || trimmed.contains('*')
        || trimmed.contains('/')
        || trimmed.contains('^')
        || trimmed.contains('%');

    // Check for function calls like sqrt(), sin(), etc.
    let has_function = trimmed.contains("sqrt")
        || trimmed.contains("sin")
        || trimmed.contains("cos")
        || trimmed.contains("tan")
        || trimmed.contains("log")
        || trimmed.contains("ln")
        || trimmed.contains("exp")
        || trimmed.contains("abs")
        || trimmed.contains("floor")
        || trimmed.contains("ceil");

    if !has_operator && !has_function {
        return None;
    }

    // Check valid start: digit, '(', '-', or '.'
    let first_char = trimmed.chars().next()?;
    let valid_start =
        first_char.is_ascii_digit() || first_char == '(' || first_char == '-' || first_char == '.';

    // Also allow function names at the start
    let starts_with_function = trimmed.starts_with("sqrt")
        || trimmed.starts_with("sin")
        || trimmed.starts_with("cos")
        || trimmed.starts_with("tan")
        || trimmed.starts_with("log")
        || trimmed.starts_with("ln")
        || trimmed.starts_with("exp")
        || trimmed.starts_with("abs")
        || trimmed.starts_with("floor")
        || trimmed.starts_with("ceil");

    if !valid_start && !starts_with_function {
        return None;
    }

    // Try to evaluate
    match meval::eval_str(trimmed) {
        Ok(result) if result.is_finite() => Some(CalculatorResult {
            expression: trimmed.to_string(),
            result,
        }),
        _ => None,
    }
}

fn fuzzy_match_configs(configs: &[RaffiConfig], query: &str) -> Vec<usize> {
    let matcher = SkimMatcherV2::default();
    let mut matches: Vec<(usize, i64)> = configs
        .iter()
        .enumerate()
        .filter_map(|(idx, config)| {
            let description = config
                .description
                .as_deref()
                .or(config.binary.as_deref())
                .unwrap_or_default();
            matcher
                .fuzzy_match(description, query)
                .map(|score| (idx, score))
        })
        .collect();

    // Sort by score descending
    matches.sort_by(|a, b| b.1.cmp(&a.1));

    matches.into_iter().map(|(idx, _)| idx).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fuzzy_match_configs() {
        let configs = vec![
            RaffiConfig {
                description: Some("Firefox".to_string()),
                ..Default::default()
            },
            RaffiConfig {
                description: Some("Google Chrome".to_string()),
                ..Default::default()
            },
            RaffiConfig {
                description: Some("Alacritty".to_string()),
                ..Default::default()
            },
            RaffiConfig {
                binary: Some("code".to_string()),
                description: None,
                ..Default::default()
            },
        ];

        // Exact match
        let results = fuzzy_match_configs(&configs, "Firefox");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0], 0);

        // Fuzzy match "fox" -> Firefox
        let results = fuzzy_match_configs(&configs, "fox");
        assert!(results.contains(&0));

        // Fuzzy match "chr" -> Google Chrome
        let results = fuzzy_match_configs(&configs, "chr");
        assert!(results.contains(&1));

        // Fuzzy match binary "od" -> code
        let results = fuzzy_match_configs(&configs, "od");
        assert!(results.contains(&3));

        // Ranking check: "o" matches Firefox, Google Chrome, code
        // "code" (idx 3) should likely be high for "o" if it starts with it or is short,
        // but let's just check we get results
        let results = fuzzy_match_configs(&configs, "o");
        assert!(results.len() >= 3);
        assert!(results.contains(&0)); // FirefOx
        assert!(results.contains(&1)); // GOOgle Chrome
        assert!(results.contains(&3)); // cOde
    }

    #[test]
    fn test_try_evaluate_math_basic_operations() {
        // Addition
        let result = try_evaluate_math("2+2");
        assert!(result.is_some());
        assert_eq!(result.as_ref().unwrap().result, 4.0);

        // Subtraction
        let result = try_evaluate_math("10-3");
        assert!(result.is_some());
        assert_eq!(result.unwrap().result, 7.0);

        // Multiplication
        let result = try_evaluate_math("5*6");
        assert!(result.is_some());
        assert_eq!(result.unwrap().result, 30.0);

        // Division
        let result = try_evaluate_math("20/4");
        assert!(result.is_some());
        assert_eq!(result.unwrap().result, 5.0);

        // Power
        let result = try_evaluate_math("2^3");
        assert!(result.is_some());
        assert_eq!(result.unwrap().result, 8.0);

        // Modulo
        let result = try_evaluate_math("17%5");
        assert!(result.is_some());
        assert_eq!(result.unwrap().result, 2.0);
    }

    #[test]
    fn test_try_evaluate_math_complex_expressions() {
        // Parentheses
        let result = try_evaluate_math("(10+5)*2");
        assert!(result.is_some());
        assert_eq!(result.unwrap().result, 30.0);

        // Nested parentheses
        let result = try_evaluate_math("((2+3)*4)-5");
        assert!(result.is_some());
        assert_eq!(result.unwrap().result, 15.0);

        // Negative numbers
        let result = try_evaluate_math("-5+10");
        assert!(result.is_some());
        assert_eq!(result.unwrap().result, 5.0);

        // Decimals
        let result = try_evaluate_math("3.5*2");
        assert!(result.is_some());
        assert_eq!(result.unwrap().result, 7.0);
    }

    #[test]
    fn test_try_evaluate_math_functions() {
        // sqrt
        let result = try_evaluate_math("sqrt(16)");
        assert!(result.is_some());
        assert_eq!(result.unwrap().result, 4.0);

        // abs
        let result = try_evaluate_math("abs(-5)");
        assert!(result.is_some());
        assert_eq!(result.unwrap().result, 5.0);

        // sin(0) should be 0
        let result = try_evaluate_math("sin(0)");
        assert!(result.is_some());
        assert!((result.unwrap().result - 0.0).abs() < 0.0001);
    }

    #[test]
    fn test_try_evaluate_math_not_math() {
        // Regular text should not trigger calculator
        assert!(try_evaluate_math("firefox").is_none());
        assert!(try_evaluate_math("google chrome").is_none());
        assert!(try_evaluate_math("hello world").is_none());

        // Text with numbers but no operators
        assert!(try_evaluate_math("firefox123").is_none());

        // Empty string
        assert!(try_evaluate_math("").is_none());

        // Just whitespace
        assert!(try_evaluate_math("   ").is_none());
    }

    #[test]
    fn test_try_evaluate_math_invalid_expressions() {
        // Division by zero produces infinity, should be rejected
        let result = try_evaluate_math("1/0");
        assert!(result.is_none());

        // Invalid syntax - operator at start (no valid start character)
        assert!(try_evaluate_math("*5").is_none());

        // Text starting with letters should not match
        assert!(try_evaluate_math("x+5").is_none());
    }
}

fn load_mru_map() -> HashMap<String, u32> {
    if let Ok(path) = super::get_mru_cache_path() {
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
    if let Ok(path) = super::get_mru_cache_path() {
        let mut entries: Vec<_> = map.iter().collect();
        entries.sort_by(|a, b| b.1.cmp(a.1));
        let content = entries
            .iter()
            .map(|(desc, count)| format!("{}|{}", desc, count))
            .collect::<Vec<_>>()
            .join("\n");
        if let Err(e) = fs::write(&path, content) {
            eprintln!("Warning: Failed to save MRU cache to {:?}: {}", path, e);
        }
    }
}

/// Run the Wayland UI with the provided configurations and return the selected item.
fn run_wayland_ui(configs: &[RaffiConfig], no_icons: bool) -> Result<String> {
    let selected_item: SharedSelection = Arc::new(Mutex::new(None));
    let selected_item_clone = selected_item.clone();

    // Clone configs to own them for the 'static lifetime requirement
    let configs_owned = configs.to_vec();

    fn new_app(
        configs_owned: Vec<RaffiConfig>,
        no_icons: bool,
        selected_item_clone: SharedSelection,
    ) -> (LauncherApp, Task<Message>) {
        LauncherApp::new(configs_owned, no_icons, selected_item_clone)
    }

    let configs_for_new = configs_owned.clone();
    let selected_item_for_new = selected_item_clone.clone();

    let window_settings = window::Settings {
        size: iced::Size::new(800.0, 600.0),
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

    let result = iced::application(
        move || {
            new_app(
                configs_for_new.clone(),
                no_icons,
                selected_item_for_new.clone(),
            )
        },
        LauncherApp::update,
        LauncherApp::view,
    )
    .subscription(LauncherApp::subscription)
    .theme(|_state: &LauncherApp| iced::Theme::Dark)
    .window(window_settings)
    .run();

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
