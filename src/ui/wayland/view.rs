use std::path::{Path, PathBuf};

use iced::widget::{
    button, container, image, mouse_area, rich_text, scrollable, span, svg, text, text_input,
    Column, Row, Space,
};
use iced::{Element, Length};

use super::ansi::ansi_to_spans;
use super::browser::mimetype_icon_name;
use super::preview::{PreviewBody, PreviewData, PreviewIcon};
use super::snippets::{truncate_preview, SnippetIcon};
use super::state::{LauncherApp, Message};
use super::theme::ThemeColors;

type HintSpan<'a> = iced::widget::text::Span<'a, (), iced::Font>;

fn item_button_style(
    t: ThemeColors,
    is_selected: bool,
    text_color: iced::Color,
) -> impl Fn(&iced::Theme, button::Status) -> button::Style {
    move |_theme, status| {
        let base_style = button::Style {
            text_color,
            border: iced::Border {
                radius: 10.0.into(),
                ..Default::default()
            },
            ..Default::default()
        };

        if is_selected {
            button::Style {
                background: Some(iced::Background::Color(t.selection_bg)),
                border: iced::Border {
                    color: iced::Color {
                        a: 0.38,
                        ..t.accent
                    },
                    width: 1.0,
                    radius: 10.0.into(),
                },
                ..base_style
            }
        } else {
            match status {
                button::Status::Hovered => button::Style {
                    background: Some(iced::Background::Color(iced::Color {
                        a: 0.12,
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
    }
}

/// Styling for file browser rows. The rows are [`mouse_area`] widgets rather
/// than buttons (pressing a row must arm a drag with the exact entry under
/// the pointer), so this replicates the item button look from tracked
/// selection and hover state.
fn item_row_style(
    t: ThemeColors,
    is_selected: bool,
    is_hovered: bool,
    text_color: iced::Color,
) -> impl Fn(&iced::Theme) -> container::Style {
    move |_theme| {
        let base_style = container::Style {
            text_color: Some(text_color),
            border: iced::Border {
                radius: 10.0.into(),
                ..Default::default()
            },
            ..Default::default()
        };

        if is_selected {
            container::Style {
                background: Some(iced::Background::Color(t.selection_bg)),
                border: iced::Border {
                    color: iced::Color {
                        a: 0.38,
                        ..t.accent
                    },
                    width: 1.0,
                    radius: 10.0.into(),
                },
                ..base_style
            }
        } else if is_hovered {
            container::Style {
                background: Some(iced::Background::Color(iced::Color {
                    a: 0.12,
                    ..t.accent_hover
                })),
                ..base_style
            }
        } else {
            base_style
        }
    }
}

/// Ctrl+. on any layout: iced reports the unshifted key in `key`, so a
/// layout where "." needs Shift only shows it in `modified_key`.
pub(super) fn is_ctrl_period(
    key: &iced::keyboard::Key,
    modified_key: &iced::keyboard::Key,
    modifiers: iced::keyboard::Modifiers,
) -> bool {
    let is_period = |key: &iced::keyboard::Key| matches!(key, iced::keyboard::Key::Character(c) if c.as_str() == ".");
    modifiers.control() && (is_period(key) || is_period(modified_key))
}

const PREVIEW_ROW_SPACING: f32 = 12.0;
const PREVIEW_DIVIDER_WIDTH: f32 = 1.0;
const PREVIEW_PANE_LEFT_PADDING: f32 = 14.0;
const PREVIEW_PANE_RIGHT_PADDING: f32 = 12.0;
const PREVIEW_CONTENT_RIGHT_PADDING: f32 = 6.0;

/// `FillPortion` weights of the list and preview panes for `width_fraction`.
fn preview_fill_portions(width_fraction: f32) -> (u16, u16) {
    let right_portion = (width_fraction * 100.0).round().max(1.0) as u16;
    let left_portion = 100u16.saturating_sub(right_portion).max(1);
    (left_portion, right_portion)
}

fn slim_scrollbar() -> scrollable::Direction {
    scrollable::Direction::Vertical(
        scrollable::Scrollbar::new()
            .width(4)
            .scroller_width(4)
            .spacing(6),
    )
}

fn scrollable_style(
    t: ThemeColors,
) -> impl Fn(&iced::Theme, scrollable::Status) -> scrollable::Style {
    move |theme, status| {
        let mut style = scrollable::default(theme, status);
        let is_active = matches!(
            status,
            scrollable::Status::Hovered {
                is_vertical_scrollbar_hovered: true,
                ..
            } | scrollable::Status::Dragged { .. }
        );
        let thumb_color = if is_active {
            iced::Color {
                a: 0.65,
                ..t.accent
            }
        } else {
            iced::Color {
                a: 0.35,
                ..t.text_muted
            }
        };
        let rail = scrollable::Rail {
            background: None,
            border: iced::Border::default(),
            scroller: scrollable::Scroller {
                background: iced::Background::Color(thumb_color),
                border: iced::Border {
                    radius: 2.0.into(),
                    ..Default::default()
                },
            },
        };
        style.vertical_rail = rail;
        style.horizontal_rail = rail;
        style.gap = None;
        style
    }
}

/// Build an icon widget for `path`, or `None` when the file is missing.
fn icon_element<'a>(path: &Path, size: f32) -> Option<Element<'a, Message>> {
    if !path.exists() {
        return None;
    }
    let is_svg = path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.eq_ignore_ascii_case("svg"))
        .unwrap_or(false);

    Some(if is_svg {
        svg(iced::widget::svg::Handle::from_path(path))
            .width(Length::Fixed(size))
            .height(Length::Fixed(size))
            .content_fit(iced::ContentFit::Contain)
            .into()
    } else {
        image(PathBuf::from(path))
            .width(Length::Fixed(size))
            .height(Length::Fixed(size))
            .content_fit(iced::ContentFit::Contain)
            .into()
    })
}

/// Build an image (or SVG) widget that fills the preview pane viewport at
/// `height`, preserving its aspect ratio. The pane width fills automatically.
fn full_pane_image<'a>(path: &str, height: f32) -> Option<Element<'a, Message>> {
    let img_path = Path::new(path);
    if !img_path.exists() {
        return None;
    }
    let is_svg = img_path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.eq_ignore_ascii_case("svg"))
        .unwrap_or(false);

    Some(if is_svg {
        svg(iced::widget::svg::Handle::from_path(img_path))
            .width(Length::Fill)
            .height(Length::Fixed(height))
            .content_fit(iced::ContentFit::Contain)
            .into()
    } else {
        image(PathBuf::from(img_path))
            .width(Length::Fill)
            .height(Length::Fixed(height))
            .content_fit(iced::ContentFit::Contain)
            .into()
    })
}

/// Image decoded from memory, sized like [`full_pane_image`].
fn memory_image<'a>(handle: iced::widget::image::Handle, height: f32) -> Element<'a, Message> {
    image(handle)
        .width(Length::Fill)
        .height(Length::Fixed(height))
        .content_fit(iced::ContentFit::Contain)
        .into()
}

impl LauncherApp {
    /// Wrap the shared window chrome around an already built item list.
    fn shell<'a>(
        &'a self,
        placeholder: &'a str,
        hint_spans: Vec<HintSpan<'a>>,
        items_column: Column<'a, Message>,
    ) -> Element<'a, Message> {
        let t = self.view.theme;
        let fs = self.view.font_sizes;

        let search_input = text_input(placeholder, &self.search_query)
            .id(self.view.search_input_id.clone())
            // Enter is handled by the subscription alone, which knows about
            // key autorepeat; an `on_submit` here would deliver it twice.
            .on_input(Message::SearchChanged)
            .padding(fs.input_padding)
            .size(fs.input)
            .style(move |_theme, status| {
                let border_color = if matches!(status, text_input::Status::Focused { .. }) {
                    iced::Color {
                        a: 0.65,
                        ..t.accent
                    }
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

        let items_container = container(items_column)
            .id(self.view.items_container_id.clone())
            .width(Length::Fill)
            .height(Length::Shrink);

        let items_scroll = scrollable(items_container)
            .id(self.view.scrollable_id.clone())
            .direction(slim_scrollbar())
            .style(scrollable_style(t))
            .height(Length::Fill)
            .width(Length::Fill);

        let footer_divider = container(Space::new())
            .width(Length::Fill)
            .height(Length::Fixed(1.0))
            .style(move |_theme| container::Style {
                background: Some(iced::Background::Color(iced::Color {
                    a: 0.45,
                    ..t.border
                })),
                ..Default::default()
            });

        let hint_row = container(rich_text(hint_spans))
            .padding(iced::Padding {
                top: 4.0,
                right: 2.0,
                bottom: 0.0,
                left: 2.0,
            })
            .width(Length::Fill)
            .align_x(iced::Alignment::End);

        let footer = Column::new()
            .spacing(4)
            .width(Length::Fill)
            .push(footer_divider)
            .push(hint_row);

        let left_pane = container(items_scroll)
            .padding(iced::Padding {
                top: fs.scroll_top_padding,
                right: 6.0,
                bottom: 0.0,
                left: 0.0,
            })
            .height(Length::Fill);

        let body_area: Element<'a, Message> =
            if let Some(preview_data) = self.resolve_selected_preview() {
                let (left_portion, right_portion) =
                    preview_fill_portions(self.view.preview_style.width_fraction);
                let divider = container(Space::new())
                    .width(Length::Fixed(PREVIEW_DIVIDER_WIDTH))
                    .height(Length::Fill)
                    .style(move |_theme| container::Style {
                        background: Some(iced::Background::Color(iced::Color {
                            a: 0.55,
                            ..t.border
                        })),
                        ..Default::default()
                    });

                let right_pane = container(self.render_preview_pane(preview_data))
                    .padding(iced::Padding {
                        top: fs.scroll_top_padding,
                        right: 0.0,
                        bottom: 0.0,
                        left: 0.0,
                    })
                    .width(Length::FillPortion(right_portion))
                    .height(Length::Fill);

                Row::new()
                    .spacing(PREVIEW_ROW_SPACING)
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .push(left_pane.width(Length::FillPortion(left_portion)))
                    .push(divider)
                    .push(right_pane)
                    .into()
            } else {
                left_pane.width(Length::Fill).into()
            };

        let content = Column::new()
            .spacing(10)
            .width(Length::Fill)
            .height(Length::Fill)
            .push(search_input)
            .push(body_area)
            .push(footer);

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

    /// Render one row per available keyword or trigger, shown with `Ctrl+/`.
    fn help_items_column(&self) -> Column<'_, Message> {
        let t = self.view.theme;
        let fs = self.view.font_sizes;

        if self.help_filtered.is_empty() {
            return Column::new().spacing(6).push(
                container(
                    text("No matching help entries.")
                        .size(fs.subtitle)
                        .color(t.text_muted),
                )
                .width(Length::Fill)
                .height(Length::Fill)
                .align_x(iced::Alignment::Center)
                .align_y(iced::Alignment::Center),
            );
        }

        // Fixed slot so rows without an icon still line up with icon rows.
        let icon_size = fs.item + 8.0;

        // Column widths derived from the visible rows so trigger, title and
        // usage align; 0.62 approximates the average glyph advance.
        let visible_entries = || {
            self.help_filtered
                .iter()
                .filter_map(|&index| self.help_entries.get(index))
        };
        let trigger_width = visible_entries()
            .map(|entry| entry.trigger.chars().count() as f32 * fs.subtitle * 0.62)
            .fold(0.0_f32, f32::max)
            + 20.0;
        let title_width = visible_entries()
            .map(|entry| entry.title.chars().count() as f32 * fs.item * 0.62)
            .fold(0.0_f32, f32::max)
            + 12.0;

        let mut items_column = Column::new().spacing(4);
        let mut previous_section = None;

        for (display_idx, &entry_index) in self.help_filtered.iter().enumerate() {
            let Some(entry) = self.help_entries.get(entry_index) else {
                continue;
            };
            let is_selected = display_idx == self.selected_index;

            if previous_section != Some(entry.section) {
                items_column = items_column.push(
                    container(
                        text(entry.section.heading())
                            .size(fs.subtitle)
                            .color(t.text_muted),
                    )
                    .width(Length::Fill)
                    .padding([8.0, fs.item_padding]),
                );
                previous_section = Some(entry.section);
            }

            let icon_slot: Element<Message> = entry
                .icon
                .as_ref()
                .and_then(|icon_name| self.icon_map.get(icon_name))
                .and_then(|icon_path| icon_element(Path::new(icon_path), icon_size))
                .unwrap_or_else(|| {
                    container(Space::new())
                        .width(Length::Fixed(icon_size))
                        .height(Length::Fixed(icon_size))
                        .into()
                });

            let trigger_pill = container(
                text(entry.trigger.clone())
                    .size(fs.subtitle)
                    .color(t.accent),
            )
            .padding([3.0, 10.0])
            .center_x(Length::Fixed(trigger_width))
            .style(move |_theme| container::Style {
                background: Some(iced::Background::Color(iced::Color {
                    a: 0.14,
                    ..t.accent
                })),
                border: iced::Border {
                    radius: 6.0.into(),
                    ..Default::default()
                },
                ..Default::default()
            });

            let title = container(text(entry.title.clone()).size(fs.item).color(t.text_main))
                .width(Length::Fixed(title_width));

            let usage = text(entry.usage.clone())
                .size(fs.subtitle)
                .color(t.text_muted);

            let item_row = Row::new()
                .spacing(12)
                .align_y(iced::Alignment::Center)
                .push(icon_slot)
                .push(trigger_pill)
                .push(title)
                .push(usage);

            let item_button = button(item_row)
                .on_press(Message::ItemClicked(display_idx))
                .padding(fs.item_padding)
                .width(Length::Fill)
                .style(item_button_style(t, is_selected, t.text_main));

            items_column = items_column.push(item_button);
        }

        items_column
    }

    fn help_hint_spans(&self) -> Vec<HintSpan<'_>> {
        let t = self.view.theme;
        let fs = self.view.font_sizes;

        vec![
            span("Enter").size(fs.hint).color(t.accent),
            span(" use keyword").size(fs.hint).color(t.text_muted),
            span("  ·  ").size(fs.hint).color(t.border),
            span("Ctrl+P").size(fs.hint).color(t.accent),
            span(" preview").size(fs.hint).color(t.text_muted),
            span("  ·  ").size(fs.hint).color(t.border),
            span("Esc").size(fs.hint).color(t.accent),
            span(" close help").size(fs.hint).color(t.text_muted),
        ]
    }

    /// Height of the preview pane viewport, derived from the live window size
    /// so image previews fill the pane and track window resizes.
    pub(super) fn preview_viewport_height(&self) -> f32 {
        let fs = self.view.font_sizes;
        // Chrome above and below the pane: window padding, hint row, search
        // input, column spacing, the body top padding and the pane's padding.
        let chrome = fs.outer_padding * 2.0
            + fs.hint
            + 4.0
            + 24.0
            + fs.input
            + fs.input_padding * 2.0
            + 2.0
            + fs.scroll_top_padding
            + 32.0;
        (self.view.window_size.height - chrome).max(160.0)
    }

    /// Width of the preview pane content, derived from the live window size
    /// the same way the layout splits the body row between list and pane.
    pub(super) fn preview_viewport_width(&self) -> f32 {
        let fs = self.view.font_sizes;
        let (left_portion, right_portion) =
            preview_fill_portions(self.view.preview_style.width_fraction);
        // The row loses the window padding, the gaps around the divider and
        // the divider itself before the panes split the rest by portion.
        let row = self.view.window_size.width
            - fs.outer_padding * 2.0
            - PREVIEW_ROW_SPACING * 2.0
            - PREVIEW_DIVIDER_WIDTH;
        let pane = row * f32::from(right_portion) / f32::from(left_portion + right_portion);
        let chrome =
            PREVIEW_PANE_LEFT_PADDING + PREVIEW_PANE_RIGHT_PADDING + PREVIEW_CONTENT_RIGHT_PADDING;
        (pane - chrome).max(160.0)
    }

    /// Render the right-hand preview pane for `data`.
    fn render_preview_pane(&self, data: PreviewData) -> Element<'_, Message> {
        let t = self.view.theme;
        let ps = self.view.preview_style.clone();
        let block_style = {
            let ps = ps.clone();
            move |_theme: &iced::Theme| container::Style {
                background: Some(iced::Background::Color(ps.bg_block)),
                border: iced::Border {
                    color: iced::Color {
                        a: 0.55,
                        ..ps.border
                    },
                    width: 1.0,
                    radius: 8.0.into(),
                },
                ..Default::default()
            }
        };
        let heading = |label: String| text(label).size(ps.small).color(ps.accent).font(ps.font);
        let body_text = |value: String| {
            text(value)
                .size(ps.text)
                .color(ps.text_main)
                .font(ps.mono_font)
        };

        let mut col = Column::new().spacing(14).width(Length::Fill);

        // Image previews fill the whole pane: the image is rendered first at the
        // full viewport size, and the header plus file info follow below the fold,
        // reachable by scrolling with Ctrl+J / Ctrl+K.
        let image_first = matches!(
            data.bodies.first(),
            Some(PreviewBody::Image { .. } | PreviewBody::ImageData { .. })
        );
        let viewport_height = self.preview_viewport_height();

        // Header: optional icon + title/subtitle + optional badge
        let mut header_row = Row::new()
            .spacing(14)
            .align_y(iced::Alignment::Center)
            .width(Length::Fill);

        if let Some(icon) = data.icon {
            match icon {
                PreviewIcon::File(path) => {
                    if let Some(el) = icon_element(Path::new(&path), ps.icon) {
                        header_row = header_row.push(el);
                    }
                }
                PreviewIcon::Glyph(glyph) => {
                    header_row = header_row.push(
                        text(glyph)
                            .size(ps.icon * 0.8)
                            .width(Length::Fixed(ps.icon)),
                    );
                }
            }
        }

        let mut title_col = Column::new().spacing(2).width(Length::Fill);
        title_col = title_col.push(
            text(data.title)
                .size(ps.title)
                .color(ps.text_main)
                .font(ps.font),
        );
        if let Some(sub) = data.subtitle {
            title_col = title_col.push(text(sub).size(ps.small).color(ps.text_muted).font(ps.font));
        }
        header_row = header_row.push(title_col);

        if let Some(badge) = data.badge {
            let accent = ps.accent;
            let badge_pill = container(text(badge).size(ps.small).color(accent).font(ps.font))
                .padding([4.0, 10.0])
                .style(move |_theme| container::Style {
                    background: Some(iced::Background::Color(iced::Color { a: 0.14, ..accent })),
                    border: iced::Border {
                        radius: 6.0.into(),
                        ..Default::default()
                    },
                    ..Default::default()
                });
            header_row = header_row.push(badge_pill);
        }

        if image_first {
            match data.bodies.first() {
                Some(PreviewBody::Image { path }) => {
                    if let Some(el) = full_pane_image(path, viewport_height) {
                        col = col.push(el);
                    }
                }
                Some(PreviewBody::ImageData { handle }) => {
                    col = col.push(memory_image(handle.clone(), viewport_height));
                }
                _ => {}
            }
        }

        col = col.push(header_row);

        // Horizontal divider under header
        let border = ps.border;
        col = col.push(
            container(Space::new())
                .width(Length::Fill)
                .height(Length::Fixed(1.0))
                .style(move |_theme| container::Style {
                    background: Some(iced::Background::Color(iced::Color { a: 0.3, ..border })),
                    ..Default::default()
                }),
        );

        // Body sections
        for (body_index, body) in data.bodies.into_iter().enumerate() {
            if image_first && body_index == 0 {
                continue; // already rendered above, filling the first view
            }
            match body {
                PreviewBody::Text {
                    heading: h,
                    content,
                    truncated,
                } => {
                    let mut section = Column::new().spacing(6).width(Length::Fill);
                    if let Some(h) = h {
                        section = section.push(heading(h));
                    }
                    section = section.push(
                        container(body_text(content))
                            .padding(12.0)
                            .width(Length::Fill)
                            .style(block_style),
                    );
                    if truncated {
                        section = section.push(
                            text("Preview truncated (showing first 8 KiB / 100 lines)")
                                .size(ps.small)
                                .color(ps.text_muted)
                                .font(ps.font),
                        );
                    }
                    col = col.push(section);
                }
                PreviewBody::AnsiText {
                    heading: h,
                    content,
                } => {
                    let mut section = Column::new().spacing(6).width(Length::Fill);
                    if let Some(h) = h {
                        section = section.push(heading(h));
                    }
                    section = section.push(
                        container(
                            rich_text(ansi_to_spans(&content, ps.text, ps.text_main))
                                .font(ps.mono_font),
                        )
                        .padding(12.0)
                        .width(Length::Fill)
                        .style(block_style),
                    );
                    col = col.push(section);
                }
                PreviewBody::Image { path } => {
                    if let Some(el) = full_pane_image(&path, viewport_height) {
                        col = col.push(el);
                    }
                }
                PreviewBody::ImageData { handle } => {
                    col = col.push(memory_image(handle, viewport_height));
                }
                PreviewBody::HeroGlyph { glyph } => {
                    col = col.push(
                        container(text(glyph).size(ps.hero_glyph))
                            .padding(20.0)
                            .width(Length::Fill)
                            .align_x(iced::Alignment::Center)
                            .align_y(iced::Alignment::Center)
                            .style(block_style),
                    );
                }
                PreviewBody::HeroValue { expression, value } => {
                    let mut inner = Column::new()
                        .spacing(6)
                        .align_x(iced::Alignment::Center)
                        .width(Length::Fill);
                    if let Some(expr) = expression {
                        inner =
                            inner.push(text(expr).size(ps.text).color(ps.text_muted).font(ps.font));
                    }
                    inner = inner.push(
                        text(value)
                            .size(ps.hero_value)
                            .color(ps.accent)
                            .font(ps.font),
                    );

                    col = col.push(
                        container(inner)
                            .padding(20.0)
                            .width(Length::Fill)
                            .align_x(iced::Alignment::Center)
                            .style(block_style),
                    );
                }
                PreviewBody::DirectoryList {
                    entries,
                    total_count,
                } => {
                    let mut section = Column::new().spacing(6).width(Length::Fill);
                    section = section.push(heading(format!("Directory Contents ({total_count})")));

                    if entries.is_empty() {
                        section = section.push(
                            text("(Empty directory)")
                                .size(ps.text)
                                .color(ps.text_muted)
                                .font(ps.font),
                        );
                    } else {
                        let mut list_col = Column::new().spacing(3).width(Length::Fill);
                        for (name, is_dir) in entries {
                            let label = if is_dir {
                                format!("\u{1f4c1} {name}/")
                            } else {
                                name
                            };
                            let color = if is_dir { ps.accent } else { ps.text_main };
                            list_col =
                                list_col.push(text(label).size(ps.text).color(color).font(ps.font));
                        }
                        if total_count > super::preview::MAX_DIR_PREVIEW_ENTRIES {
                            list_col = list_col.push(
                                text(format!(
                                    "... and {} more",
                                    total_count - super::preview::MAX_DIR_PREVIEW_ENTRIES
                                ))
                                .size(ps.small)
                                .color(ps.text_muted)
                                .font(ps.font),
                            );
                        }
                        section = section.push(
                            container(list_col)
                                .padding(12.0)
                                .width(Length::Fill)
                                .style(block_style),
                        );
                    }
                    col = col.push(section);
                }
            }
        }

        // Metadata key-value rows
        if !data.metadata.is_empty() {
            let mut meta_col = Column::new().spacing(8).width(Length::Fill);
            meta_col = meta_col.push(heading("Information".to_string()));
            for (k, v) in data.metadata {
                let row = Row::new()
                    .spacing(10)
                    .align_y(iced::Alignment::Start)
                    .width(Length::Fill)
                    .push(
                        container(text(k).size(ps.small).color(ps.text_muted).font(ps.font))
                            .width(Length::Fixed(ps.key_width)),
                    )
                    .push(
                        text(v)
                            .size(ps.text)
                            .color(ps.text_main)
                            .font(ps.font)
                            .width(Length::Fill),
                    );
                meta_col = meta_col.push(row);
            }
            col = col.push(meta_col);
        }

        let scroll = scrollable(col.padding(iced::Padding {
            top: 0.0,
            right: PREVIEW_CONTENT_RIGHT_PADDING,
            bottom: 0.0,
            left: 0.0,
        }))
        .id(self.view.preview_scrollable_id.clone())
        .direction(slim_scrollbar())
        .style(scrollable_style(t))
        .width(Length::Fill)
        .height(Length::Fill);

        let pane_bg = ps.bg;
        container(scroll)
            .padding(iced::Padding {
                top: 8.0,
                right: PREVIEW_PANE_RIGHT_PADDING,
                bottom: 8.0,
                left: PREVIEW_PANE_LEFT_PADDING,
            })
            .width(Length::Fill)
            .height(Length::Fill)
            .style(move |_theme| container::Style {
                background: Some(iced::Background::Color(pane_bg)),
                border: iced::Border {
                    radius: 10.0.into(),
                    ..Default::default()
                },
                ..Default::default()
            })
            .into()
    }

    pub(super) fn view(&self) -> Element<'_, Message> {
        let t = self.view.theme;
        let fs = self.view.font_sizes;

        if self.view.help_active {
            return self.shell(
                "Search help...",
                self.help_hint_spans(),
                self.help_items_column(),
            );
        }

        let mut items_column = Column::new().spacing(6);

        let has_script_filter = self.script_filter.results.is_some()
            || self.script_filter.loading
            || self.script_filter.help_message.is_some();
        let has_text_snippet = self.text_snippets.active || self.text_snippets.loading;
        let has_emoji = self.emoji.active;
        let has_clipboard = self.clipboard.active;
        let has_file_browser = self.file_browser.active;
        let has_web_search = self.web_search.active.is_some();
        let has_currency = self.currency.result.is_some()
            || self.currency.loading
            || self.currency.multi_result.is_some()
            || self.currency.multi_loading;
        let has_inline = self
            .inline_result
            .as_ref()
            .is_some_and(|inline| !inline.rows.is_empty());

        let mut special_item_idx = 0;

        for suggestion in &self.keyword_suggestions {
            let display_idx = special_item_idx;
            let is_selected = display_idx == self.selected_index;
            let mut item_row = Row::new().spacing(16).align_y(iced::Alignment::Center);

            if let Some(icon_name) = &suggestion.icon {
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

            item_row = item_row
                .push(
                    text(suggestion.keyword.clone())
                        .size(fs.item)
                        .color(t.accent),
                )
                .push(
                    text(suggestion.subtitle.clone())
                        .size(fs.item)
                        .color(t.text_main),
                );

            let item_button = button(item_row)
                .on_press(Message::ItemClicked(display_idx))
                .padding(fs.item_padding)
                .width(Length::Fill)
                .style(item_button_style(t, is_selected, t.text_main));

            items_column = items_column.push(item_button);
            special_item_idx += 1;
        }

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
                .style(item_button_style(t, is_selected, t.text_muted));

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
                .style(item_button_style(t, is_selected, t.text_muted));

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
                    .style(item_button_style(t, is_selected, t.text_main));

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
                .style(item_button_style(t, is_selected, t.text_muted));

            items_column = items_column.push(loading_button);
            special_item_idx += 1;
        } else if self.text_snippets.active {
            if let Some(error) = &self.text_snippets.error {
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

            for &snippet_idx in &self.text_snippets.filtered {
                if let Some(snippet) = self.text_snippets.items.get(snippet_idx) {
                    let is_selected = self.selected_index == special_item_idx;
                    let mut item_row = Row::new().spacing(16).align_y(iced::Alignment::Center);

                    if let Some(icon) = self.snippet_icon(snippet) {
                        match icon {
                            SnippetIcon::File(path) => {
                                if let Some(icon) = icon_element(Path::new(&path), 40.0) {
                                    item_row = item_row.push(icon);
                                }
                            }
                            SnippetIcon::Glyph(glyph) => {
                                item_row = item_row.push(
                                    text(glyph).size(fs.item + 4.0).width(Length::Fixed(40.0)),
                                );
                            }
                        }
                    }

                    let mut text_col = Column::new();
                    let mut title_row = Row::new()
                        .spacing(8)
                        .align_y(iced::Alignment::Center)
                        .push(text(&snippet.name).size(fs.item).color(t.text_main));
                    if let Some(keyword) = &snippet.keyword {
                        title_row =
                            title_row.push(text(keyword).size(fs.subtitle).color(t.text_muted));
                    }
                    text_col = text_col.push(title_row);
                    let preview = self
                        .text_snippets
                        .previews
                        .get(snippet_idx)
                        .and_then(Option::as_deref)
                        .unwrap_or(&snippet.value);
                    text_col = text_col.push(
                        text(truncate_preview(preview))
                            .size(fs.subtitle)
                            .color(t.text_muted),
                    );
                    item_row = item_row.push(text_col.width(Length::Fill));

                    let item_button = button(item_row)
                        .on_press(Message::ItemClicked(special_item_idx))
                        .padding(fs.item_padding)
                        .width(Length::Fill)
                        .style(item_button_style(t, is_selected, t.text_main));

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
                        .style(item_button_style(t, is_selected, t.text_main));

                    items_column = items_column.push(item_button);
                    special_item_idx += 1;
                }
            }
        }

        if self.clipboard.active {
            let state = &self.clipboard;
            if state.items.is_empty() {
                let is_selected = self.selected_index == special_item_idx;
                let status_row = Row::new()
                    .spacing(16)
                    .align_y(iced::Alignment::Center)
                    .push(text(state.status()).size(fs.item).color(t.text_muted));
                items_column = items_column.push(
                    button(status_row)
                        .padding(fs.item_padding)
                        .width(Length::Fill)
                        .style(item_button_style(t, is_selected, t.text_muted)),
                );
                special_item_idx += 1;
            } else if let Some(error) = &state.error {
                let error_row = Row::new()
                    .spacing(16)
                    .align_y(iced::Alignment::Center)
                    .push(text(error.clone()).size(fs.item).color(t.text_muted));
                items_column = items_column.push(
                    button(error_row)
                        .padding(fs.item_padding)
                        .width(Length::Fill)
                        .style(move |_theme, _status| button::Style {
                            text_color: t.text_muted,
                            border: iced::Border {
                                radius: 8.0.into(),
                                ..Default::default()
                            },
                            ..Default::default()
                        }),
                );
            }

            for item in &state.items {
                let is_selected = self.selected_index == special_item_idx;
                let text_col = Column::new()
                    .push(text(item.title()).size(fs.item).color(t.text_main))
                    .push(text(item.subtitle()).size(fs.subtitle).color(t.text_muted));
                let item_row = Row::new()
                    .spacing(16)
                    .align_y(iced::Alignment::Center)
                    .push(text_col.width(Length::Fill));
                items_column = items_column.push(
                    button(item_row)
                        .on_press(Message::ItemClicked(special_item_idx))
                        .padding(fs.item_padding)
                        .width(Length::Fill)
                        .style(item_button_style(t, is_selected, t.text_main)),
                );
                special_item_idx += 1;
            }
        }

        if let Some(status) = self.command_list.status() {
            let is_selected = self.selected_index == special_item_idx;
            let status_row = Row::new()
                .spacing(16)
                .align_y(iced::Alignment::Center)
                .push(text(status.to_string()).size(fs.item).color(t.text_muted));
            items_column = items_column.push(
                button(status_row)
                    .padding(fs.item_padding)
                    .width(Length::Fill)
                    .style(item_button_style(t, is_selected, t.text_muted)),
            );
            special_item_idx += 1;
        } else if self.command_list.is_active() {
            for row in 0..self.command_list.row_count() {
                let Some(item) = self.command_list.item(row) else {
                    continue;
                };
                let is_selected = self.selected_index == special_item_idx;
                let mut item_row = Row::new().spacing(16).align_y(iced::Alignment::Center);
                if let Some(icon) = item
                    .icon
                    .as_deref()
                    .and_then(|path| icon_element(Path::new(path), 40.0))
                {
                    item_row = item_row.push(icon);
                }
                let subtitle = if let Some(reason) = &item.unavailable {
                    text(reason.clone()).size(fs.subtitle).color(t.text_muted)
                } else if let Some(progress) = self.command_list_progress(item) {
                    text(progress.to_string()).size(fs.subtitle).color(t.accent)
                } else if let Some(verb) = self.command_list.armed_verb(item) {
                    text(format!("Press Enter again to {verb}"))
                        .size(fs.subtitle)
                        .color(t.accent)
                } else {
                    text(item.subtitle.clone())
                        .size(fs.subtitle)
                        .color(t.text_muted)
                };
                let text_col = Column::new()
                    .push(text(item.title.clone()).size(fs.item).color(t.text_main))
                    .push(subtitle);
                item_row = item_row.push(text_col.width(Length::Fill));
                items_column = items_column.push(
                    button(item_row)
                        .on_press(Message::ItemClicked(special_item_idx))
                        .padding(fs.item_padding)
                        .width(Length::Fill)
                        .style(item_button_style(t, is_selected, t.text_main)),
                );
                special_item_idx += 1;
            }
        }

        if self.devtools.active {
            for row in &self.devtools.rows {
                let is_selected = self.selected_index == special_item_idx;
                let mut item_row = Row::new().spacing(16).align_y(iced::Alignment::Center);
                if let Some(icon) = self
                    .icon_map
                    .get(super::app::DEVTOOLS_ICON)
                    .and_then(|path| icon_element(Path::new(path), 40.0))
                {
                    item_row = item_row.push(icon);
                }
                // Multi-line results such as pretty JSON show their first line.
                let first_line = row.value.lines().next().unwrap_or_default();
                let shown = if first_line.len() < row.value.len() {
                    format!("{first_line} ...")
                } else {
                    first_line.to_string()
                };
                let subtitle_color = if row.copyable { t.accent } else { t.text_muted };
                let text_col = Column::new()
                    .push(text(row.title).size(fs.item).color(t.text_main))
                    .push(text(shown).size(fs.subtitle).color(subtitle_color));
                item_row = item_row.push(text_col.width(Length::Fill));
                let mut item_button = button(item_row)
                    .padding(fs.item_padding)
                    .width(Length::Fill)
                    .style(item_button_style(t, is_selected, t.text_main));
                if row.copyable {
                    item_button = item_button.on_press(Message::ItemClicked(special_item_idx));
                }
                items_column = items_column.push(item_button);
                special_item_idx += 1;
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

            for (entry_index, entry) in self.file_browser.entries.iter().enumerate() {
                let is_selected = self.selected_index == special_item_idx;
                let is_hovered = self.file_browser.hovered_row == Some(entry_index);
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

                // Rows are mouse areas instead of buttons: the press must
                // carry the exact entry under the pointer so it can arm a
                // drag-and-drop session, and a release on the same row then
                // activates it like a button click.
                let item_container = container(item_row)
                    .padding(fs.item_padding)
                    .width(Length::Fill)
                    .style(item_row_style(t, is_selected, is_hovered, name_color));

                let item_area = mouse_area(item_container)
                    .on_enter(Message::FileBrowserHover(entry_index))
                    .on_exit(Message::FileBrowserUnhover(entry_index))
                    .on_press(Message::FileBrowserRowPressed {
                        index: special_item_idx,
                        path: entry.full_path.clone(),
                    })
                    .on_release(Message::FileBrowserRowReleased(special_item_idx))
                    .interaction(iced::mouse::Interaction::Pointer);

                items_column = items_column.push(item_area);
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
                    .style(item_button_style(t, is_selected, t.text_muted));

                items_column = items_column.push(ws_button);
            } else {
                let label = format!("Search {} for '{}'", ws.name, ws.query);
                ws_row = ws_row.push(text(label).size(fs.item).color(t.accent));

                let ws_button = button(ws_row)
                    .on_press(Message::WebSearchSelected)
                    .padding(fs.item_padding)
                    .width(Length::Fill)
                    .style(item_button_style(t, is_selected, t.accent));

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
                .style(item_button_style(t, is_selected, t.text_muted));

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
                .style(item_button_style(t, is_selected, t.text_muted));

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
                .style(item_button_style(t, is_selected, t.accent));

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
                .style(item_button_style(t, is_selected, t.text_muted));

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
                    .style(item_button_style(t, is_selected, t.accent));

                items_column = items_column.push(conversion_button);
                special_item_idx += 1;
            }
        }

        if let Some(inline) = &self.inline_result {
            for (idx, inline_row) in inline.rows.iter().enumerate() {
                let is_selected = self.selected_index == special_item_idx;
                let mut row = Row::new().spacing(16).align_y(iced::Alignment::Center);
                if let Some(color) = inline_row.swatch {
                    row = row.push(
                        container(Space::new())
                            .width(Length::Fixed(fs.item))
                            .height(Length::Fixed(fs.item))
                            .style(move |_theme| container::Style {
                                background: Some(iced::Background::Color(color)),
                                border: iced::Border {
                                    color: t.border,
                                    width: 1.0,
                                    radius: 4.0.into(),
                                },
                                ..Default::default()
                            }),
                    );
                }
                row = row.push(
                    text(inline_row.title.as_str())
                        .size(fs.item)
                        .color(t.accent),
                );
                if let Some(subtitle) = &inline_row.subtitle {
                    row = row.push(
                        text(subtitle.as_str())
                            .size(fs.subtitle)
                            .color(t.text_muted),
                    );
                }

                let inline_button = button(row)
                    .on_press(Message::InlineSelected(idx))
                    .padding(fs.item_padding)
                    .width(Length::Fill)
                    .style(item_button_style(t, is_selected, t.accent));

                items_column = items_column.push(inline_button);
                special_item_idx += 1;
            }
        }

        if let Some(error) = &self.inline_copy_error {
            let error_row = Row::new()
                .spacing(16)
                .align_y(iced::Alignment::Center)
                .push(text(error.as_str()).size(fs.item).color(t.text_muted));
            items_column = items_column.push(
                button(error_row)
                    .padding(fs.item_padding)
                    .width(Length::Fill)
                    .style(move |_theme, _status| button::Style {
                        text_color: t.text_muted,
                        border: iced::Border {
                            radius: 8.0.into(),
                            ..Default::default()
                        },
                        ..Default::default()
                    }),
            );
        }

        let has_fallbacks = self.fallback_count() > 0;
        let has_keyword_suggestions = !self.keyword_suggestions.is_empty();

        if self.filtered_configs.is_empty()
            && !has_inline
            && !has_currency
            && !has_script_filter
            && !has_text_snippet
            && !has_emoji
            && !has_clipboard
            && self.command_list.row_count() == 0
            && self.devtools.rows.is_empty()
            && !has_file_browser
            && !has_web_search
            && !has_fallbacks
            && !has_keyword_suggestions
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

                let mut item_row = Row::new().spacing(16).align_y(iced::Alignment::Center);
                if let Some(icon_path_str) = self.launcher_icon_paths[config_idx].clone() {
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
                    .style(item_button_style(t, is_selected, t.text_main));

                items_column = items_column.push(item_button);
            }
        }

        if has_fallbacks {
            let fallback_start_idx = special_item_idx + self.filtered_configs.len();
            for (fb_idx, fallback) in self.fallbacks.iter().enumerate() {
                let display_idx = fallback_start_idx + fb_idx;
                let is_selected = display_idx == self.selected_index;
                let mut item_row = Row::new().spacing(16).align_y(iced::Alignment::Center);

                if let Some(icon_name) = &fallback.icon {
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

                let label = format!(
                    "Search {} for '{}'",
                    fallback.name,
                    self.search_query.trim()
                );
                let fb_text_color = if is_selected {
                    t.text_main
                } else {
                    t.text_muted
                };
                item_row = item_row.push(text(label).size(fs.item).color(fb_text_color));

                let item_button = button(item_row)
                    .on_press(Message::ItemClicked(display_idx))
                    .padding(fs.item_padding)
                    .width(Length::Fill)
                    .style(item_button_style(t, is_selected, fb_text_color));

                items_column = items_column.push(item_button);
            }
        }

        let sep = span("  ·  ").size(fs.hint).color(t.border);
        let mut hint_spans: Vec<HintSpan<'_>> = Vec::new();

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
            hint_spans.push(sep.clone());
            hint_spans.push(span("Ctrl+O").size(fs.hint).color(t.accent));
            hint_spans.push(span(" file manager").size(fs.hint).color(t.text_muted));
            hint_spans.push(sep.clone());
            hint_spans.push(span("Ctrl+S").size(fs.hint).color(t.accent));
            hint_spans.push(
                span(format!(
                    " sort: {}{}",
                    self.file_browser.sort.label(),
                    if self.file_browser.sort_reverse {
                        " (reversed)"
                    } else {
                        ""
                    }
                ))
                .size(fs.hint)
                .color(t.text_muted),
            );
            hint_spans.push(sep.clone());
            hint_spans.push(span("Ctrl+.").size(fs.hint).color(t.accent));
            hint_spans.push(
                span(format!(" show: {}", self.file_browser.show.label()))
                    .size(fs.hint)
                    .color(t.text_muted),
            );
        }
        if self.clipboard.active {
            if !hint_spans.is_empty() {
                hint_spans.push(sep.clone());
            }
            if self.clipboard.loading && !self.clipboard.items.is_empty() {
                hint_spans.push(
                    span(format!(
                        "scanning {}/{}",
                        self.clipboard.scanned, self.clipboard.total
                    ))
                    .size(fs.hint)
                    .color(t.text_muted),
                );
                hint_spans.push(sep.clone());
            }
            hint_spans.push(span("Ctrl+R").size(fs.hint).color(t.accent));
            hint_spans.push(span(" refresh").size(fs.hint).color(t.text_muted));
            hint_spans.push(sep.clone());
            hint_spans.push(span("Ctrl+I").size(fs.hint).color(t.accent));
            let images = if self.clipboard.images_only {
                " all items"
            } else {
                " images only"
            };
            hint_spans.push(span(images).size(fs.hint).color(t.text_muted));
        }
        if !hint_spans.is_empty() {
            hint_spans.push(sep.clone());
        }
        hint_spans.push(span("Ctrl+P").size(fs.hint).color(t.accent));
        hint_spans.push(span(" preview").size(fs.hint).color(t.text_muted));
        hint_spans.push(sep.clone());
        hint_spans.push(span("Ctrl+/").size(fs.hint).color(t.accent));
        hint_spans.push(span(" keywords").size(fs.hint).color(t.text_muted));

        if self.file_browser.dragging {
            hint_spans.clear();
            hint_spans.push(
                span("Release over a target to drop the file")
                    .size(fs.hint)
                    .color(t.accent),
            );
        }

        self.shell("Type to search...", hint_spans, items_column)
    }

    pub(super) fn subscription(&self) -> iced::Subscription<Message> {
        use iced::keyboard;
        use iced::keyboard::key::Named;
        use iced::mouse;
        use iced::window;
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
                repeat,
                ..
            }) => Some(if repeat {
                Message::SubmitRepeat
            } else {
                Message::Submit
            }),
            Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(Named::Escape),
                ..
            }) => Some(Message::Cancel),
            Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(Named::Tab),
                ..
            }) => Some(Message::TabComplete),
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
            }) if c.as_str() == "o" && modifiers.control() => {
                Some(Message::FileBrowserOpenFileManager)
            }
            Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Character(ref c),
                modifiers,
                ..
            }) if c.as_str() == "s" && modifiers.control() => Some(Message::FileBrowserCycleSort),
            Event::Keyboard(keyboard::Event::KeyPressed {
                ref key,
                ref modified_key,
                modifiers,
                ..
            }) if is_ctrl_period(key, modified_key, modifiers) => {
                Some(Message::FileBrowserCycleShow)
            }
            Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Character(ref c),
                modifiers,
                ..
            }) if c.as_str() == "/" && modifiers.control() => Some(Message::ToggleHelp),
            Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Character(ref c),
                modifiers,
                ..
            }) if c.as_str() == "p" && modifiers.control() => Some(Message::TogglePreview),
            Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Character(ref c),
                modifiers,
                ..
            }) if c.as_str() == "j" && modifiers.control() => Some(Message::PreviewScrollDown),
            Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Character(ref c),
                modifiers,
                ..
            }) if c.as_str() == "k" && modifiers.control() => Some(Message::PreviewScrollUp),
            Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Character(ref c),
                modifiers,
                ..
            }) if c.as_str() == "r" && modifiers.control() => Some(Message::ClipboardRefresh),
            Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Character(ref c),
                modifiers,
                ..
            }) if c.as_str() == "i" && modifiers.control() => Some(Message::ClipboardToggleImages),
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
                Some(Message::MouseReleased)
            }
            // iced's mouse_area maps touch lifts to the same press/release
            // callbacks, so a finger lifted outside the rows must reset row
            // press tracking just like a mouse button release.
            Event::Touch(iced::touch::Event::FingerLifted { .. })
            | Event::Touch(iced::touch::Event::FingerLost { .. }) => Some(Message::MouseReleased),
            Event::Window(window::Event::Resized(size)) => Some(Message::WindowResized(size)),
            Event::Keyboard(keyboard::Event::ModifiersChanged(modifiers)) => {
                Some(Message::ModifiersChanged(modifiers))
            }
            _ => None,
        })
    }
}
