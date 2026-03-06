use std::path::{Path, PathBuf};

use iced::widget::{
    button, container, image, rich_text, scrollable, span, svg, text, text_input, Column, Row,
};
use iced::{Element, Length};

use super::ansi::ansi_to_spans;
use super::browser::mimetype_icon_name;
use super::state::{LauncherApp, Message};

impl LauncherApp {
    pub(super) fn view(&self) -> Element<'_, Message> {
        let t = self.view.theme;
        let fs = self.view.font_sizes;

        let search_input = text_input("Type to search...", &self.search_query)
            .id(self.view.search_input_id.clone())
            .on_input(Message::SearchChanged)
            .on_submit(Message::Submit)
            .padding(fs.input_padding)
            .size(fs.input)
            .style(move |_theme, status| {
                let border_color = if matches!(status, text_input::Status::Focused { .. }) {
                    t.accent
                } else {
                    t.border
                };

                text_input::Style {
                    background: iced::Background::Color(t.bg_input),
                    border: iced::Border {
                        radius: 12.0.into(),
                        width: 1.0,
                        color: border_color,
                    },
                    placeholder: t.text_muted,
                    value: t.text_main,
                    selection: t.accent,
                    icon: t.text_muted,
                }
            })
            .width(Length::Fill);

        let mut items_column = Column::new().spacing(6);

        let has_script_filter = self.script_filter.results.is_some()
            || self.script_filter.loading
            || self.script_filter.help_message.is_some();
        let has_text_snippet = self.text_snippets.active || self.text_snippets.loading;
        let has_emoji = self.emoji.active;
        let has_file_browser = self.file_browser.active;
        let has_web_search = self.web_search.active.is_some();
        let has_currency = self.currency.result.is_some()
            || self.currency.loading
            || self.currency.multi_result.is_some()
            || self.currency.multi_loading;
        let has_calculator = self.calculator_result.is_some();

        let mut special_item_idx = 0;

        if self.script_filter.loading {
            let loading_name = self
                .script_filter
                .loading_name
                .as_deref()
                .unwrap_or("script filter");
            let loading_text = format!("Loading {}...", loading_name);

            let loading_row = Row::new()
                .spacing(16)
                .align_y(iced::Alignment::Center)
                .push(text(loading_text).size(fs.item).color(t.text_muted));

            let is_selected = self.selected_index == special_item_idx;
            let loading_button = button(loading_row)
                .padding(fs.item_padding)
                .width(Length::Fill)
                .style(move |_theme, _status| {
                    let base_style = button::Style {
                        text_color: t.text_muted,
                        border: iced::Border {
                            radius: 8.0.into(),
                            ..Default::default()
                        },
                        ..Default::default()
                    };

                    if is_selected {
                        button::Style {
                            background: Some(iced::Background::Color(t.selection_bg)),
                            border: iced::Border {
                                color: t.accent,
                                width: 1.0,
                                radius: 8.0.into(),
                            },
                            ..base_style
                        }
                    } else {
                        button::Style {
                            background: None,
                            ..base_style
                        }
                    }
                });

            items_column = items_column.push(loading_button);
            special_item_idx += 1;
        } else if let Some(help_msg) = &self.script_filter.help_message {
            let help_row = Row::new()
                .spacing(16)
                .align_y(iced::Alignment::Center)
                .push(text(help_msg.clone()).size(fs.item).color(t.text_muted));

            let is_selected = self.selected_index == special_item_idx;
            let help_button = button(help_row)
                .padding(fs.item_padding)
                .width(Length::Fill)
                .style(move |_theme, _status| {
                    let base_style = button::Style {
                        text_color: t.text_muted,
                        border: iced::Border {
                            radius: 8.0.into(),
                            ..Default::default()
                        },
                        ..Default::default()
                    };

                    if is_selected {
                        button::Style {
                            background: Some(iced::Background::Color(t.selection_bg)),
                            border: iced::Border {
                                color: t.accent,
                                width: 1.0,
                                radius: 8.0.into(),
                            },
                            ..base_style
                        }
                    } else {
                        button::Style {
                            background: None,
                            ..base_style
                        }
                    }
                });

            items_column = items_column.push(help_button);
            special_item_idx += 1;
        } else if let Some(sf_result) = &self.script_filter.results {
            for item in &sf_result.items {
                let is_selected = self.selected_index == special_item_idx;
                let mut item_row = Row::new().spacing(16).align_y(iced::Alignment::Center);

                let icon_path = item
                    .icon
                    .as_ref()
                    .and_then(|icon| icon.path.clone())
                    .and_then(|path| {
                        let expanded = crate::expand_config_value(&path);
                        if Path::new(&expanded).exists() {
                            Some(expanded)
                        } else {
                            None
                        }
                    })
                    .or_else(|| {
                        sf_result
                            .default_icon
                            .as_ref()
                            .and_then(|name| self.icon_map.get(name).cloned())
                    });

                if let Some(icon_path_str) = icon_path {
                    let icon_path = PathBuf::from(&icon_path_str);
                    if icon_path.exists() {
                        let is_svg = icon_path
                            .extension()
                            .and_then(|ext| ext.to_str())
                            .map(|ext| ext.eq_ignore_ascii_case("svg"))
                            .unwrap_or(false);

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

                let mut text_col = Column::new();
                text_col =
                    text_col.push(rich_text(ansi_to_spans(&item.title, fs.item, t.text_main)));
                if let Some(subtitle) = &item.subtitle {
                    text_col = text_col.push(rich_text(ansi_to_spans(
                        subtitle,
                        fs.subtitle,
                        t.text_muted,
                    )));
                }
                item_row = item_row.push(text_col.width(Length::Fill));

                let item_button = button(item_row)
                    .on_press(Message::ItemClicked(special_item_idx))
                    .padding(fs.item_padding)
                    .width(Length::Fill)
                    .style(move |_theme, status| {
                        let base_style = button::Style {
                            text_color: t.text_main,
                            border: iced::Border {
                                radius: 8.0.into(),
                                ..Default::default()
                            },
                            ..Default::default()
                        };

                        if is_selected {
                            button::Style {
                                background: Some(iced::Background::Color(t.selection_bg)),
                                border: iced::Border {
                                    color: t.accent,
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
                                        ..t.accent_hover
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

                items_column = items_column.push(item_button);
                special_item_idx += 1;
            }
        }

        if self.text_snippets.loading {
            let loading_row = Row::new()
                .spacing(16)
                .align_y(iced::Alignment::Center)
                .push(
                    text("Loading snippets...")
                        .size(fs.item)
                        .color(t.text_muted),
                );

            let is_selected = self.selected_index == special_item_idx;
            let loading_button = button(loading_row)
                .padding(fs.item_padding)
                .width(Length::Fill)
                .style(move |_theme, _status| {
                    let base_style = button::Style {
                        text_color: t.text_muted,
                        border: iced::Border {
                            radius: 8.0.into(),
                            ..Default::default()
                        },
                        ..Default::default()
                    };

                    if is_selected {
                        button::Style {
                            background: Some(iced::Background::Color(t.selection_bg)),
                            border: iced::Border {
                                color: t.accent,
                                width: 1.0,
                                radius: 8.0.into(),
                            },
                            ..base_style
                        }
                    } else {
                        button::Style {
                            background: None,
                            ..base_style
                        }
                    }
                });

            items_column = items_column.push(loading_button);
            special_item_idx += 1;
        } else if self.text_snippets.active {
            for &snippet_idx in &self.text_snippets.filtered {
                if let Some(snippet) = self.text_snippets.items.get(snippet_idx) {
                    let is_selected = self.selected_index == special_item_idx;
                    let mut item_row = Row::new().spacing(16).align_y(iced::Alignment::Center);

                    if let Some(icon_name) = &self.text_snippets.icon {
                        if let Some(icon_path_str) = self.icon_map.get(icon_name) {
                            let icon_path = PathBuf::from(icon_path_str);
                            if icon_path.exists() {
                                let is_svg = icon_path
                                    .extension()
                                    .and_then(|ext| ext.to_str())
                                    .map(|ext| ext.eq_ignore_ascii_case("svg"))
                                    .unwrap_or(false);

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
                    }

                    let mut text_col = Column::new();
                    text_col = text_col.push(text(&snippet.name).size(fs.item).color(t.text_main));
                    let truncated_value = if snippet.value.len() > 80 {
                        format!("{}...", &snippet.value[..80])
                    } else {
                        snippet.value.clone()
                    };
                    text_col =
                        text_col.push(text(truncated_value).size(fs.subtitle).color(t.text_muted));
                    item_row = item_row.push(text_col.width(Length::Fill));

                    let item_button = button(item_row)
                        .on_press(Message::ItemClicked(special_item_idx))
                        .padding(fs.item_padding)
                        .width(Length::Fill)
                        .style(move |_theme, status| {
                            let base_style = button::Style {
                                text_color: t.text_main,
                                border: iced::Border {
                                    radius: 8.0.into(),
                                    ..Default::default()
                                },
                                ..Default::default()
                            };

                            if is_selected {
                                button::Style {
                                    background: Some(iced::Background::Color(t.selection_bg)),
                                    border: iced::Border {
                                        color: t.accent,
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
                                            ..t.accent_hover
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

                    items_column = items_column.push(item_button);
                    special_item_idx += 1;
                }
            }
        }

        if self.emoji.active {
            for &emoji_idx in &self.emoji.filtered {
                if let Some(entry) = self.emoji.data.get(emoji_idx) {
                    let is_selected = self.selected_index == special_item_idx;

                    let item_row = Row::new()
                        .spacing(16)
                        .align_y(iced::Alignment::Center)
                        .push(
                            text(entry.value.as_str())
                                .size(fs.item + 4.0)
                                .width(Length::Fixed(40.0)),
                        )
                        .push(
                            text(entry.name.as_str())
                                .size(fs.item)
                                .color(t.text_main)
                                .width(Length::Fill),
                        );

                    let item_button = button(item_row)
                        .on_press(Message::ItemClicked(special_item_idx))
                        .padding(fs.item_padding)
                        .width(Length::Fill)
                        .style(move |_theme, status| {
                            let base_style = button::Style {
                                text_color: t.text_main,
                                border: iced::Border {
                                    radius: 8.0.into(),
                                    ..Default::default()
                                },
                                ..Default::default()
                            };

                            if is_selected {
                                button::Style {
                                    background: Some(iced::Background::Color(t.selection_bg)),
                                    border: iced::Border {
                                        color: t.accent,
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
                                            ..t.accent_hover
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

                    items_column = items_column.push(item_button);
                    special_item_idx += 1;
                }
            }
        }

        if self.file_browser.active {
            if let Some(error) = &self.file_browser.error {
                let error_row = Row::new()
                    .spacing(16)
                    .align_y(iced::Alignment::Center)
                    .push(text(error.clone()).size(fs.item).color(t.text_muted));

                let error_button = button(error_row)
                    .padding(fs.item_padding)
                    .width(Length::Fill)
                    .style(move |_theme, _status| button::Style {
                        text_color: t.text_muted,
                        border: iced::Border {
                            radius: 8.0.into(),
                            ..Default::default()
                        },
                        ..Default::default()
                    });

                items_column = items_column.push(error_button);
            }

            for entry in &self.file_browser.entries {
                let is_selected = self.selected_index == special_item_idx;
                let mut item_row = Row::new().spacing(16).align_y(iced::Alignment::Center);

                if !self.icon_map.is_empty() {
                    let icon_name = if entry.is_dir {
                        "folder"
                    } else {
                        mimetype_icon_name(&entry.full_path)
                    };
                    if let Some(icon_path_str) = self.icon_map.get(icon_name) {
                        let icon_path = PathBuf::from(icon_path_str);
                        if icon_path.exists() {
                            let is_svg = icon_path
                                .extension()
                                .and_then(|ext| ext.to_str())
                                .map(|ext| ext.eq_ignore_ascii_case("svg"))
                                .unwrap_or(false);

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
                }

                let display_name = if entry.is_dir {
                    format!("{}/", entry.name)
                } else {
                    entry.name.clone()
                };
                let name_color = if entry.is_dir { t.accent } else { t.text_main };

                let mut text_col = Column::new();
                text_col = text_col.push(text(display_name).size(fs.item).color(name_color));
                text_col = text_col.push(
                    text(entry.full_path.clone())
                        .size(fs.subtitle)
                        .color(t.text_muted),
                );
                item_row = item_row.push(text_col.width(Length::Fill));

                let item_button = button(item_row)
                    .on_press(Message::ItemClicked(special_item_idx))
                    .padding(fs.item_padding)
                    .width(Length::Fill)
                    .style(move |_theme, status| {
                        let base_style = button::Style {
                            text_color: t.text_main,
                            border: iced::Border {
                                radius: 8.0.into(),
                                ..Default::default()
                            },
                            ..Default::default()
                        };

                        if is_selected {
                            button::Style {
                                background: Some(iced::Background::Color(t.selection_bg)),
                                border: iced::Border {
                                    color: t.accent,
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
                                        ..t.accent_hover
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

                items_column = items_column.push(item_button);
                special_item_idx += 1;
            }
        }

        if let Some(ws) = &self.web_search.active {
            let is_selected = self.selected_index == special_item_idx;
            let mut ws_row = Row::new().spacing(16).align_y(iced::Alignment::Center);

            if let Some(icon_name) = &ws.icon {
                if let Some(icon_path_str) = self.icon_map.get(icon_name) {
                    let icon_path = PathBuf::from(icon_path_str);
                    if icon_path.exists() {
                        let is_svg = icon_path
                            .extension()
                            .and_then(|ext| ext.to_str())
                            .map(|ext| ext.eq_ignore_ascii_case("svg"))
                            .unwrap_or(false);

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
                        ws_row = ws_row.push(icon_content);
                    }
                }
            }

            if ws.query.is_empty() {
                let hint = format!("Search {}: type your query...", ws.name);
                ws_row = ws_row.push(text(hint).size(fs.item).color(t.text_muted));

                let ws_button = button(ws_row)
                    .padding(fs.item_padding)
                    .width(Length::Fill)
                    .style(move |_theme, _status| {
                        let base_style = button::Style {
                            text_color: t.text_muted,
                            border: iced::Border {
                                radius: 8.0.into(),
                                ..Default::default()
                            },
                            ..Default::default()
                        };

                        if is_selected {
                            button::Style {
                                background: Some(iced::Background::Color(t.selection_bg)),
                                border: iced::Border {
                                    color: t.accent,
                                    width: 1.0,
                                    radius: 8.0.into(),
                                },
                                ..base_style
                            }
                        } else {
                            button::Style {
                                background: None,
                                ..base_style
                            }
                        }
                    });

                items_column = items_column.push(ws_button);
            } else {
                let label = format!("Search {} for '{}'", ws.name, ws.query);
                ws_row = ws_row.push(text(label).size(fs.item).color(t.accent));

                let ws_button = button(ws_row)
                    .on_press(Message::WebSearchSelected)
                    .padding(fs.item_padding)
                    .width(Length::Fill)
                    .style(move |_theme, status| {
                        let base_style = button::Style {
                            text_color: t.accent,
                            border: iced::Border {
                                radius: 8.0.into(),
                                ..Default::default()
                            },
                            ..Default::default()
                        };

                        if is_selected {
                            button::Style {
                                background: Some(iced::Background::Color(t.selection_bg)),
                                border: iced::Border {
                                    color: t.accent,
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
                                        ..t.accent_hover
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

                items_column = items_column.push(ws_button);
            }
            special_item_idx += 1;
        }

        if self.currency.help {
            let is_selected = self.selected_index == special_item_idx;
            let help_row = Row::new()
                .spacing(16)
                .align_y(iced::Alignment::Center)
                .push(
                    text("Currency: $10 to EUR, $50 GBP to USD")
                        .size(fs.item)
                        .color(t.text_muted),
                );

            let help_button = button(help_row)
                .padding(fs.item_padding)
                .width(Length::Fill)
                .style(move |_theme, _status| {
                    let base_style = button::Style {
                        text_color: t.text_muted,
                        border: iced::Border {
                            radius: 8.0.into(),
                            ..Default::default()
                        },
                        ..Default::default()
                    };

                    if is_selected {
                        button::Style {
                            background: Some(iced::Background::Color(t.selection_bg)),
                            border: iced::Border {
                                color: t.accent,
                                width: 1.0,
                                radius: 8.0.into(),
                            },
                            ..base_style
                        }
                    } else {
                        button::Style {
                            background: None,
                            ..base_style
                        }
                    }
                });

            items_column = items_column.push(help_button);
            special_item_idx += 1;
        }

        if self.currency.loading {
            let loading_text = if let Some(request) = &self.currency.pending_request {
                format!(
                    "Converting {} {} to {}...",
                    request.amount, request.from_currency, request.to_currency
                )
            } else {
                "Converting...".to_string()
            };

            let is_selected = self.selected_index == special_item_idx;
            let loading_row = Row::new()
                .spacing(16)
                .align_y(iced::Alignment::Center)
                .push(text(loading_text).size(fs.item).color(t.text_muted));

            let loading_button = button(loading_row)
                .padding(fs.item_padding)
                .width(Length::Fill)
                .style(move |_theme, _status| {
                    let base_style = button::Style {
                        text_color: t.text_muted,
                        border: iced::Border {
                            radius: 8.0.into(),
                            ..Default::default()
                        },
                        ..Default::default()
                    };

                    if is_selected {
                        button::Style {
                            background: Some(iced::Background::Color(t.selection_bg)),
                            border: iced::Border {
                                color: t.accent,
                                width: 1.0,
                                radius: 8.0.into(),
                            },
                            ..base_style
                        }
                    } else {
                        button::Style {
                            background: None,
                            ..base_style
                        }
                    }
                });

            items_column = items_column.push(loading_button);
            special_item_idx += 1;
        } else if let Some(currency) = &self.currency.result {
            let currency_text = format!(
                "{:.2} {} = {:.2} {} (rate: {:.4})",
                currency.request.amount,
                currency.request.from_currency,
                currency.converted_amount,
                currency.request.to_currency,
                currency.rate
            );

            let is_selected = self.selected_index == special_item_idx;
            let currency_row = Row::new()
                .spacing(16)
                .align_y(iced::Alignment::Center)
                .push(text(currency_text).size(fs.item).color(t.accent));

            let currency_button = button(currency_row)
                .on_press(Message::CurrencyResultCopied)
                .padding(fs.item_padding)
                .width(Length::Fill)
                .style(move |_theme, status| {
                    let base_style = button::Style {
                        text_color: t.accent,
                        border: iced::Border {
                            radius: 8.0.into(),
                            ..Default::default()
                        },
                        ..Default::default()
                    };

                    if is_selected {
                        button::Style {
                            background: Some(iced::Background::Color(t.selection_bg)),
                            border: iced::Border {
                                color: t.accent,
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
                                    ..t.accent_hover
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

            items_column = items_column.push(currency_button);
            special_item_idx += 1;
        }

        if self.currency.multi_loading {
            let loading_text = if let Some(request) = &self.currency.pending_multi_request {
                format!(
                    "Converting {} {} to {}...",
                    request.amount,
                    request.from_currency,
                    request.to_currencies.join(", ")
                )
            } else {
                "Converting...".to_string()
            };

            let is_selected = self.selected_index == special_item_idx;
            let loading_row = Row::new()
                .spacing(16)
                .align_y(iced::Alignment::Center)
                .push(text(loading_text).size(fs.item).color(t.text_muted));

            let loading_button = button(loading_row)
                .padding(fs.item_padding)
                .width(Length::Fill)
                .style(move |_theme, _status| {
                    let base_style = button::Style {
                        text_color: t.text_muted,
                        border: iced::Border {
                            radius: 8.0.into(),
                            ..Default::default()
                        },
                        ..Default::default()
                    };

                    if is_selected {
                        button::Style {
                            background: Some(iced::Background::Color(t.selection_bg)),
                            border: iced::Border {
                                color: t.accent,
                                width: 1.0,
                                radius: 8.0.into(),
                            },
                            ..base_style
                        }
                    } else {
                        button::Style {
                            background: None,
                            ..base_style
                        }
                    }
                });

            items_column = items_column.push(loading_button);
            special_item_idx += 1;
        } else if let Some(result) = &self.currency.multi_result {
            for (idx, conversion) in result.conversions.iter().enumerate() {
                let conversion_text = format!(
                    "{:.2} {} = {:.2} {} (rate: {:.4})",
                    result.amount,
                    result.from_currency,
                    conversion.converted_amount,
                    conversion.to_currency,
                    conversion.rate
                );

                let is_selected = self.selected_index == special_item_idx;
                let conversion_row = Row::new()
                    .spacing(16)
                    .align_y(iced::Alignment::Center)
                    .push(text(conversion_text).size(fs.item).color(t.accent));

                let conversion_button = button(conversion_row)
                    .on_press(Message::MultiCurrencyResultCopied(idx))
                    .padding(fs.item_padding)
                    .width(Length::Fill)
                    .style(move |_theme, status| {
                        let base_style = button::Style {
                            text_color: t.accent,
                            border: iced::Border {
                                radius: 8.0.into(),
                                ..Default::default()
                            },
                            ..Default::default()
                        };

                        if is_selected {
                            button::Style {
                                background: Some(iced::Background::Color(t.selection_bg)),
                                border: iced::Border {
                                    color: t.accent,
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
                                        ..t.accent_hover
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

                items_column = items_column.push(conversion_button);
                special_item_idx += 1;
            }
        }

        if let Some(calc) = &self.calculator_result {
            let result_str = if calc.result.fract() == 0.0 {
                format!("{}", calc.result as i64)
            } else {
                format!("{}", calc.result)
            };

            let calc_text = format!("= {} = {}", calc.expression, result_str);
            let is_selected = self.selected_index == special_item_idx;
            let calc_row = Row::new()
                .spacing(16)
                .align_y(iced::Alignment::Center)
                .push(text(calc_text).size(fs.item).color(t.accent));

            let calc_button = button(calc_row)
                .on_press(Message::CalculatorSelected)
                .padding(fs.item_padding)
                .width(Length::Fill)
                .style(move |_theme, status| {
                    let base_style = button::Style {
                        text_color: t.accent,
                        border: iced::Border {
                            radius: 8.0.into(),
                            ..Default::default()
                        },
                        ..Default::default()
                    };

                    if is_selected {
                        button::Style {
                            background: Some(iced::Background::Color(t.selection_bg)),
                            border: iced::Border {
                                color: t.accent,
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
                                    ..t.accent_hover
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
            special_item_idx += 1;
        }

        if self.filtered_configs.is_empty()
            && !has_calculator
            && !has_currency
            && !has_script_filter
            && !has_text_snippet
            && !has_emoji
            && !has_file_browser
            && !has_web_search
        {
            let no_results = container(
                text("No matching results found.")
                    .size(fs.subtitle)
                    .color(t.text_muted),
            )
            .width(Length::Fill)
            .height(Length::Fill)
            .align_x(iced::Alignment::Center)
            .align_y(iced::Alignment::Center);
            items_column = items_column.push(no_results);
        } else {
            for (idx, &config_idx) in self.filtered_configs.iter().enumerate() {
                let display_idx = idx + special_item_idx;
                let config = &self.configs[config_idx];
                let description = config
                    .description
                    .clone()
                    .unwrap_or_else(|| config.binary.clone().unwrap_or_default());

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

                let mut item_row = Row::new().spacing(16).align_y(iced::Alignment::Center);
                if let Some(icon_path_str) = icon_path {
                    let icon_path = PathBuf::from(&icon_path_str);
                    if icon_path.exists() {
                        let is_svg = icon_path
                            .extension()
                            .and_then(|ext| ext.to_str())
                            .map(|ext| ext.eq_ignore_ascii_case("svg"))
                            .unwrap_or(false);

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

                item_row = item_row.push(text(description).size(fs.item).width(Length::Fill));

                let is_selected = display_idx == self.selected_index;
                let item_button = button(item_row)
                    .on_press(Message::ItemClicked(display_idx))
                    .padding(fs.item_padding)
                    .width(Length::Fill)
                    .style(move |_theme, status| {
                        let base_style = button::Style {
                            text_color: t.text_main,
                            border: iced::Border {
                                radius: 8.0.into(),
                                ..Default::default()
                            },
                            ..Default::default()
                        };

                        if is_selected {
                            button::Style {
                                background: Some(iced::Background::Color(t.selection_bg)),
                                border: iced::Border {
                                    color: t.accent,
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
                                        ..t.accent_hover
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

                items_column = items_column.push(item_button);
            }
        }

        let items_container = container(items_column)
            .id(self.view.items_container_id.clone())
            .width(Length::Fill)
            .height(Length::Shrink);

        let items_scroll = scrollable(items_container)
            .id(self.view.scrollable_id.clone())
            .height(Length::Fill)
            .width(Length::Fill);

        let sep = span("  ·  ").size(fs.hint).color(t.border);
        let mut hint_spans: Vec<iced::widget::text::Span<'_, (), iced::Font>> = Vec::new();

        if self.history.max_items > 0 {
            hint_spans.push(span("Alt+P").size(fs.hint).color(t.accent));
            hint_spans.push(span("/").size(fs.hint).color(t.text_muted));
            hint_spans.push(span("N").size(fs.hint).color(t.accent));
            hint_spans.push(span(" history").size(fs.hint).color(t.text_muted));
        }
        if self.file_browser.active {
            if !hint_spans.is_empty() {
                hint_spans.push(sep.clone());
            }
            hint_spans.push(span("Ctrl+H").size(fs.hint).color(t.accent));
            hint_spans.push(span(" toggle hidden").size(fs.hint).color(t.text_muted));
        }
        if !hint_spans.is_empty() {
            hint_spans.push(sep.clone());
        }
        hint_spans.push(span("Ctrl+/").size(fs.hint).color(t.accent));
        hint_spans.push(span(" help").size(fs.hint).color(t.text_muted));

        if self.view.show_hints {
            let mut addon_spans: Vec<iced::widget::text::Span<'_, (), iced::Font>> = Vec::new();
            if self.addons.calculator.enabled {
                addon_spans.push(span("math").size(fs.hint).color(t.text_muted));
            }
            if self.addons.currency.enabled {
                let trigger = self.addons.currency.trigger.as_deref().unwrap_or("$");
                if !addon_spans.is_empty() {
                    addon_spans.push(sep.clone());
                }
                addon_spans.push(span(trigger.to_string()).size(fs.hint).color(t.accent));
                addon_spans.push(span(" currency").size(fs.hint).color(t.text_muted));
            }
            if self.addons.file_browser.enabled {
                if !addon_spans.is_empty() {
                    addon_spans.push(sep.clone());
                }
                addon_spans.push(span("/").size(fs.hint).color(t.accent));
                addon_spans.push(span(" files").size(fs.hint).color(t.text_muted));
            }
            for script_filter in &self.addons.script_filters {
                if !addon_spans.is_empty() {
                    addon_spans.push(sep.clone());
                }
                addon_spans.push(
                    span(script_filter.keyword.clone())
                        .size(fs.hint)
                        .color(t.accent),
                );
                addon_spans.push(
                    span(format!(" {}", script_filter.name.to_lowercase()))
                        .size(fs.hint)
                        .color(t.text_muted),
                );
            }
            for text_snippet in &self.addons.text_snippets {
                if !addon_spans.is_empty() {
                    addon_spans.push(sep.clone());
                }
                addon_spans.push(
                    span(text_snippet.keyword.clone())
                        .size(fs.hint)
                        .color(t.accent),
                );
                addon_spans.push(
                    span(format!(" {}", text_snippet.name.to_lowercase()))
                        .size(fs.hint)
                        .color(t.text_muted),
                );
            }
            for web_search in &self.addons.web_searches {
                if !addon_spans.is_empty() {
                    addon_spans.push(sep.clone());
                }
                addon_spans.push(
                    span(web_search.keyword.clone())
                        .size(fs.hint)
                        .color(t.accent),
                );
                addon_spans.push(
                    span(format!(" {}", web_search.name.to_lowercase()))
                        .size(fs.hint)
                        .color(t.text_muted),
                );
            }
            if self.addons.emoji.enabled {
                let emoji_trigger = self.addons.emoji.trigger.as_deref().unwrap_or("emoji");
                if !addon_spans.is_empty() {
                    addon_spans.push(sep.clone());
                }
                addon_spans.push(
                    span(emoji_trigger.to_string())
                        .size(fs.hint)
                        .color(t.accent),
                );
                addon_spans.push(span(" emoji & icons").size(fs.hint).color(t.text_muted));
            }
            if !addon_spans.is_empty() {
                hint_spans.push(span("\n").size(fs.hint));
                hint_spans.extend(addon_spans);
            }
        }

        let mut main_column = Column::new()
            .spacing(12)
            .width(Length::Fill)
            .height(Length::Fill);

        let hint_row = container(rich_text(hint_spans))
            .width(Length::Fill)
            .align_x(iced::Alignment::End);
        main_column = main_column.push(hint_row);

        let content = main_column
            .push(search_input)
            .push(container(items_scroll).padding(iced::Padding {
                top: fs.scroll_top_padding,
                right: 4.0,
                bottom: 0.0,
                left: 0.0,
            }));

        container(content)
            .padding(fs.outer_padding)
            .width(Length::Fill)
            .height(Length::Fill)
            .style(move |_theme| container::Style {
                background: Some(iced::Background::Color(t.bg_base)),
                border: iced::Border {
                    color: t.border,
                    width: 1.0,
                    radius: 16.0.into(),
                },
                text_color: Some(t.text_main),
                ..Default::default()
            })
            .into()
    }

    pub(super) fn subscription(&self) -> iced::Subscription<Message> {
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
            Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(Named::Tab),
                ..
            }) => Some(Message::FileBrowserTabComplete),
            Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Character(ref c),
                modifiers,
                ..
            }) if c.as_str() == "p" && modifiers.alt() => Some(Message::HistoryPrevious),
            Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Character(ref c),
                modifiers,
                ..
            }) if c.as_str() == "n" && modifiers.alt() => Some(Message::HistoryNext),
            Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Character(ref c),
                modifiers,
                ..
            }) if c.as_str() == "h" && modifiers.control() => {
                Some(Message::FileBrowserToggleHidden)
            }
            Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Character(ref c),
                modifiers,
                ..
            }) if c.as_str() == "/" && modifiers.control() => Some(Message::ToggleHints),
            Event::Keyboard(keyboard::Event::ModifiersChanged(modifiers)) => {
                Some(Message::ModifiersChanged(modifiers))
            }
            _ => None,
        })
    }
}
