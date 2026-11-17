use std::path::{Path, PathBuf};

use iced::advanced::{
    layout, mouse, overlay, renderer, widget, Clipboard, Layout, Renderer as _, Shell, Widget,
};
use iced::widget::{
    button, container, image, mouse_area, rich_text, scrollable, span, svg, text, text_input,
    Column, Row, Space,
};
use iced::{Element, Length};

use super::ansi::ansi_to_spans;
use super::browser::mimetype_icon_name;
use super::main_view::{MainRowRef, Source as MainViewSource};
use super::preview::{
    parse_mrkdwn_blocks, parse_mrkdwn_spans, ChatEntry, ChatMessageGroup, MrkdwnBlock, MrkdwnStyle,
    PreviewBody, PreviewData, PreviewIcon,
};
use super::snippets::{truncate_preview, SnippetIcon};
use super::state::{LauncherApp, Message, ScriptFilterResult};
use super::theme::{PreviewStyle, ThemeColors};
use super::types::ScriptFilterItem;

type HintSpan<'a> = iced::widget::text::Span<'a, (), iced::Font>;

/// The emoji that precedes the footer hint whose key is `key`.
fn hint_icon(key: &str) -> Option<&'static str> {
    Some(match key {
        ACTIONS_KEY => "⚡",
        "Ctrl+/" => "🔎",
        "Ctrl+Tab" => "🔀",
        "Alt+P" => "🕘",
        "Ctrl+R" => "🔄",
        "Ctrl+D" => "🗑️",
        "Ctrl+P" => "👁️",
        _ => return None,
    })
}

/// The key drawn for opening the actions of the selected result.
pub(super) const ACTIONS_KEY: &str = "➜";

/// Text between the hints of the footer.
const HINT_SEP: &str = "  ·  ";

/// Rank from which a footer hint is never given up.
const KEEP_RANK: u8 = 9;

/// How soon a footer hint is given up when the window is narrow: lower goes
/// first. The way to the actions and to the key list, and hints that are not
/// listed, such as errors and warnings, are kept.
fn hint_drop_rank(key: &str) -> u8 {
    match key {
        "Ctrl+P" => 1,
        "Alt+P" | "Ctrl+H" | "Ctrl+S" | "Ctrl+Alt+Y" => 2,
        "Ctrl+R" | "F5" | "Ctrl+B" | "Ctrl+Y" | "Ctrl+U" | "Ctrl+O" => 3,
        "Ctrl+Tab" => 4,
        "Ctrl+D" => 5,
        "Ctrl+Enter" => 6,
        ACTIONS_KEY | "Ctrl+/" => KEEP_RANK,
        _ => u8::MAX,
    }
}

/// Rough width of `text` set at `size`; emoji and symbols count wider.
fn text_width(text: &str, size: f32) -> f32 {
    text.chars()
        .map(|c| if c.is_ascii() { 0.52 } else { 1.25 } * size)
        .sum()
}

/// Colours of the keyboard key look of the footer's keys.
#[derive(Clone, Copy)]
pub(super) struct KeyCap {
    /// The colour the footer draws keys in; spans of it become keys.
    pub accent: iced::Color,
    pub background: iced::Color,
    pub border: iced::Color,
}

impl KeyCap {
    pub(super) fn new(accent: iced::Color) -> Self {
        Self {
            accent,
            background: iced::Color { a: 0.14, ..accent },
            border: iced::Color { a: 0.4, ..accent },
        }
    }

    fn is_key(&self, hint: &HintSpan<'_>) -> bool {
        hint.color == Some(self.accent) && !hint.text.starts_with(' ')
    }

    /// `hint` drawn as a key: thin spaces keep the text off the cap's edge.
    fn style<'a>(&self, hint: HintSpan<'a>) -> HintSpan<'a> {
        let text = format!("\u{2009}{}\u{2009}", hint.text);
        let mut hint = hint
            .padding([1.0, 2.0])
            .background(self.background)
            .border(iced::Border {
                color: self.border,
                width: 1.0,
                radius: 4.0.into(),
            });
        hint.text = text.into();
        hint
    }
}

/// The footer hints of `spans` made to fit `width`. They are shortened in
/// steps: the icons go first (kept only when `icons` is set), then the hints
/// of the lowest `hint_drop_rank` one by one. Keys are drawn as `keycap` keys.
pub(super) fn fit_hints<'a>(
    spans: Vec<HintSpan<'a>>,
    width: f32,
    size: f32,
    icons: bool,
    keycap: Option<KeyCap>,
) -> Vec<HintSpan<'a>> {
    let mut sep = None;
    let mut groups: Vec<Vec<HintSpan<'a>>> = vec![Vec::new()];
    for hint in spans {
        if hint.text == HINT_SEP {
            sep.get_or_insert(hint);
            groups.push(Vec::new());
        } else if let Some(group) = groups.last_mut() {
            group.push(hint);
        }
    }
    groups.retain(|group| !group.is_empty());
    let sep = sep.unwrap_or_else(|| span(HINT_SEP).size(size));
    let sep_width = text_width(HINT_SEP, size);
    let icon_of = |group: &[HintSpan<'a>]| group.first().and_then(|hint| hint_icon(&hint.text));
    let rank_of = |group: &[HintSpan<'a>]| {
        group
            .first()
            .map_or(u8::MAX, |hint| hint_drop_rank(&hint.text))
    };
    let group_width = |group: &[HintSpan<'a>], icons: bool| {
        let icon = if icons && icon_of(group).is_some() {
            text_width("🔎 ", size)
        } else {
            0.0
        };
        icon + group
            .iter()
            .map(|hint| {
                let cap = keycap.is_some_and(|cap| cap.is_key(hint));
                text_width(&hint.text, hint.size.map_or(size, |pixels| pixels.0))
                    + if cap { 0.7 * size } else { 0.0 }
            })
            .sum::<f32>()
    };
    let total = |groups: &[Vec<HintSpan<'a>>], icons: bool| {
        groups
            .iter()
            .map(|group| group_width(group, icons))
            .sum::<f32>()
            + sep_width * groups.len().saturating_sub(1) as f32
    };

    let mut icons = icons;
    while total(&groups, icons) > width {
        if icons {
            icons = false;
            continue;
        }
        let droppable = groups
            .iter()
            .enumerate()
            .filter(|(_, group)| rank_of(group) < KEEP_RANK)
            .min_by_key(|(index, group)| (rank_of(group), std::cmp::Reverse(*index)))
            .map(|(index, _)| index);
        match droppable {
            Some(index) => {
                groups.remove(index);
            }
            None => break,
        }
    }

    let mut out = Vec::new();
    for (index, group) in groups.into_iter().enumerate() {
        if index > 0 {
            out.push(sep.clone());
        }
        if let Some(icon) = icons.then(|| icon_of(&group)).flatten() {
            out.push(span(format!("{icon} ")).size(size));
        }
        for hint in group {
            match keycap {
                Some(cap) if cap.is_key(&hint) => {
                    out.push(cap.style(hint));
                    out.push(span("\u{2009}").size(size));
                }
                _ => out.push(hint),
            }
        }
    }
    out
}

/// Horizontal shift of one tree-view indentation level.
const INDENT_STEP: f32 = 24.0;
/// Highest indentation level rendered; deeper levels keep this shift.
const MAX_INDENT: u32 = 10;
const TREE_GUIDE_THICKNESS: f32 = 1.0;
const ITEM_SPACING: f32 = 6.0;

/// Left padding added to an item row for tree-view indentation.
pub(super) fn indent_left(indent: u32) -> f32 {
    indent.min(MAX_INDENT) as f32 * INDENT_STEP
}

/// Maps a printable key press to a vim command. `modified_key` carries the
/// shifted character, so `G` and `I` stay distinct from `g` and `i`.
pub(super) fn vim_key_message(event: &iced::keyboard::Event) -> Option<Message> {
    use iced::keyboard;

    match event {
        keyboard::Event::KeyPressed {
            modified_key: keyboard::Key::Character(c),
            modifiers,
            ..
        } if !modifiers.control() && !modifiers.alt() && !modifiers.logo() => {
            let mut characters = c.as_str().chars();
            match (characters.next(), characters.next()) {
                (Some(character), None) => Some(Message::VimKey(character)),
                _ => None,
            }
        }
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct TreeGuideSegments {
    pub(super) above: bool,
    pub(super) below: bool,
    pub(super) elbow: bool,
}

pub(super) fn tree_guide_segments(items: &[ScriptFilterItem]) -> Vec<Vec<TreeGuideSegments>> {
    let mut parents = vec![None; items.len()];
    let mut siblings = vec![false; items.len()];
    let mut stack: Vec<usize> = Vec::new();
    for (index, item) in items.iter().enumerate() {
        while let Some(&previous) = stack.last() {
            if items[previous].indent < item.indent {
                break;
            }
            stack.pop();
            if items[previous].indent == item.indent {
                siblings[previous] = true;
            }
        }
        parents[index] = stack.last().copied();
        stack.push(index);
    }

    let mut guides: Vec<Vec<TreeGuideSegments>> = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        let mut segments = vec![
            TreeGuideSegments {
                above: false,
                below: false,
                elbow: false
            };
            item.indent.min(MAX_INDENT) as usize
        ];
        if let Some(parent) = parents[index] {
            for (segment, inherited) in segments.iter_mut().zip(&guides[parent]) {
                segment.above = inherited.below;
                segment.below = inherited.below;
            }
            if item.indent <= MAX_INDENT {
                segments[item.indent as usize - 1] = TreeGuideSegments {
                    above: true,
                    below: siblings[index],
                    elbow: true,
                };
            }
        }
        guides.push(segments);
    }
    guides
}

fn with_tree_guide<'a>(
    row: impl Into<Element<'a, Message>>,
    segments: Vec<TreeGuideSegments>,
    padding: f32,
    theme: ThemeColors,
) -> Element<'a, Message> {
    Element::new(TreeGuides {
        content: row.into(),
        segments,
        padding,
        color: iced::Color {
            a: theme.border.a * 0.75,
            ..theme.border
        },
    })
}

struct TreeGuides<'a> {
    content: Element<'a, Message>,
    segments: Vec<TreeGuideSegments>,
    padding: f32,
    color: iced::Color,
}

impl Widget<Message, iced::Theme, iced::Renderer> for TreeGuides<'_> {
    fn tag(&self) -> widget::tree::Tag {
        self.content.as_widget().tag()
    }

    fn state(&self) -> widget::tree::State {
        self.content.as_widget().state()
    }

    fn children(&self) -> Vec<widget::Tree> {
        self.content.as_widget().children()
    }

    fn diff(&self, tree: &mut widget::Tree) {
        self.content.as_widget().diff(tree);
    }

    fn size(&self) -> iced::Size<Length> {
        self.content.as_widget().size()
    }

    fn size_hint(&self) -> iced::Size<Length> {
        self.content.as_widget().size_hint()
    }

    fn layout(
        &mut self,
        tree: &mut widget::Tree,
        renderer: &iced::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.content.as_widget_mut().layout(tree, renderer, limits)
    }

    fn update(
        &mut self,
        tree: &mut widget::Tree,
        event: &iced::Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &iced::Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &iced::Rectangle,
    ) {
        self.content.as_widget_mut().update(
            tree, event, layout, cursor, renderer, clipboard, shell, viewport,
        );
    }

    fn draw(
        &self,
        tree: &widget::Tree,
        renderer: &mut iced::Renderer,
        theme: &iced::Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &iced::Rectangle,
    ) {
        self.content
            .as_widget()
            .draw(tree, renderer, theme, style, layout, cursor, viewport);
        let bounds = layout.bounds();
        let center = bounds.center_y();
        for (level, segment) in self.segments.iter().enumerate() {
            let x = bounds.x + self.padding + level as f32 * INDENT_STEP + INDENT_STEP / 2.0;
            let top = if segment.above {
                bounds.y - ITEM_SPACING / 2.0
            } else {
                center
            };
            let bottom = if segment.below {
                bounds.y + bounds.height + ITEM_SPACING / 2.0
            } else {
                center
            };
            for rect in [
                iced::Rectangle::new(
                    iced::Point::new(x, top),
                    iced::Size::new(TREE_GUIDE_THICKNESS, bottom - top),
                ),
                iced::Rectangle::new(
                    iced::Point::new(x, center),
                    iced::Size::new(
                        if segment.elbow {
                            INDENT_STEP / 2.0
                        } else {
                            0.0
                        },
                        TREE_GUIDE_THICKNESS,
                    ),
                ),
            ] {
                if rect.width > 0.0 && rect.height > 0.0 {
                    if let Some(bounds) = rect.intersection(viewport) {
                        renderer.fill_quad(
                            renderer::Quad {
                                bounds,
                                ..Default::default()
                            },
                            self.color,
                        );
                    }
                }
            }
        }
    }

    fn mouse_interaction(
        &self,
        tree: &widget::Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &iced::Rectangle,
        renderer: &iced::Renderer,
    ) -> mouse::Interaction {
        self.content
            .as_widget()
            .mouse_interaction(tree, layout, cursor, viewport, renderer)
    }

    fn operate(
        &mut self,
        tree: &mut widget::Tree,
        layout: Layout<'_>,
        renderer: &iced::Renderer,
        operation: &mut dyn widget::Operation,
    ) {
        self.content
            .as_widget_mut()
            .operate(tree, layout, renderer, operation);
    }

    fn overlay<'a>(
        &'a mut self,
        tree: &'a mut widget::Tree,
        layout: Layout<'a>,
        renderer: &iced::Renderer,
        viewport: &iced::Rectangle,
        translation: iced::Vector,
    ) -> Option<overlay::Element<'a, Message, iced::Theme, iced::Renderer>> {
        self.content
            .as_widget_mut()
            .overlay(tree, layout, renderer, viewport, translation)
    }
}

/// F5 refreshes the view through the server, Ctrl+Shift+F5 every
/// source it knows.
pub(super) fn server_refresh_message(modifiers: iced::keyboard::Modifiers) -> Option<Message> {
    use iced::keyboard::Modifiers;
    if modifiers.is_empty() {
        Some(Message::ServerRefresh(false))
    } else if modifiers == Modifiers::CTRL | Modifiers::SHIFT {
        Some(Message::ServerRefresh(true))
    } else {
        None
    }
}

pub(super) fn file_browser_shortcut_message(
    character: &str,
    modifiers: iced::keyboard::Modifiers,
) -> Option<Message> {
    match character {
        "o" if modifiers.control() => Some(Message::FileBrowserOpenSelected),
        "b" if modifiers.control() => Some(Message::FileBrowserOpenFileManager),
        "y" if modifiers.control() && modifiers.alt() => Some(Message::FileBrowserCopyImage),
        "y" if modifiers.control() => Some(Message::FileBrowserCopyPath),
        _ => None,
    }
}

fn item_button_style(
    t: ThemeColors,
    is_selected: bool,
    text_color: iced::Color,
) -> impl Fn(&iced::Theme, button::Status) -> button::Style {
    move |_theme, _status| {
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
            // No hover highlight: a second lit row next to the keyboard
            // selection is confusing when the pointer just rests there.
            button::Style {
                background: None,
                ..base_style
            }
        }
    }
}

/// Styling for a keyword chip. Only the chip Enter arms gets a pill, so the
/// strip reads as one row with a single accent.
fn chip_style(
    t: ThemeColors,
    is_active: bool,
    row_selected: bool,
) -> impl Fn(&iced::Theme, button::Status) -> button::Style {
    move |_theme, status| {
        let accent = |a: f32| iced::Color { a, ..t.accent };
        let (background, border) = if is_active {
            (
                row_selected.then_some(iced::Background::Color(accent(0.28))),
                iced::Border {
                    color: accent(if row_selected { 1.0 } else { 0.35 }),
                    width: if row_selected { 2.0 } else { 1.0 },
                    radius: 999.0.into(),
                },
            )
        } else {
            (
                (status == button::Status::Hovered).then_some(iced::Background::Color(
                    iced::Color {
                        a: 0.12,
                        ..t.accent_hover
                    },
                )),
                iced::Border {
                    radius: 999.0.into(),
                    ..Default::default()
                },
            )
        };
        button::Style {
            background,
            text_color: t.text_main,
            border,
            ..Default::default()
        }
    }
}

const CHIP_SPACING: f32 = 4.0;
const CHIP_PADDING_X: f32 = 10.0;
const CHIP_ICON_SPACING: f32 = 6.0;
const CHIP_MARKER_WIDTH: f32 = 20.0;
/// Average glyph width in ems, on the wide side so a chip is never cut.
const CHIP_GLYPH_EM: f32 = 0.66;

/// Estimated width of a chip: iced has no cheap text measurement in `view`.
fn chip_width(keyword: &str, icon_size: Option<f32>, font_size: f32) -> f32 {
    let label = keyword.chars().count() as f32 * font_size * CHIP_GLYPH_EM;
    let icon = icon_size.map_or(0.0, |size| size + CHIP_ICON_SPACING);
    2.0 * CHIP_PADDING_X + icon + label
}

/// Width of the chips in `window`, with their spacing and edge markers.
fn chips_width(widths: &[f32], window: std::ops::Range<usize>) -> f32 {
    let chips: f32 = widths[window.clone()].iter().sum();
    let markers = [window.start > 0, window.end < widths.len()]
        .iter()
        .filter(|cut| **cut)
        .count() as f32;
    chips
        + CHIP_SPACING * window.len().saturating_sub(1) as f32
        + markers * (CHIP_MARKER_WIDTH + CHIP_SPACING)
}

/// The chips that fit in `available`, around `active`. The window grows
/// alternately right and left so both neighbours stay in view, and it keeps
/// room for a "‹" / "›" marker on each side it cuts.
pub(super) fn chip_window(widths: &[f32], available: f32, active: usize) -> std::ops::Range<usize> {
    let count = widths.len();
    if count == 0 {
        return 0..0;
    }
    let active = active.min(count - 1);
    let width = |start: usize, end: usize| chips_width(widths, start..end);
    if width(0, count) <= available {
        return 0..count;
    }
    let (mut start, mut end) = (active, active + 1);
    loop {
        let grow_right = end < count && width(start, end + 1) <= available;
        if grow_right {
            end += 1;
        }
        let grow_left = start > 0 && width(start - 1, end) <= available;
        if grow_left {
            start -= 1;
        }
        if !grow_right && !grow_left {
            return start..end;
        }
    }
}

/// Styling for file browser rows. The rows are [`mouse_area`] widgets rather
/// than buttons (pressing a row must arm a drag with the exact entry under
/// the pointer), so this replicates the item button look from tracked
/// selection state.
fn item_row_style(
    t: ThemeColors,
    is_selected: bool,
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
        } else {
            base_style
        }
    }
}

const PREVIEW_ROW_SPACING: f32 = 12.0;
const PREVIEW_DIVIDER_WIDTH: f32 = 1.0;
/// Room kept on the right of result rows for the actions chevron.
const ACTION_CHEVRON_GUTTER: f32 = 18.0;

/// Padding of a result row, with room on the right for the actions chevron.
fn row_padding(fs: crate::ui::FontSizes) -> iced::Padding {
    iced::Padding {
        right: fs.item_padding + ACTION_CHEVRON_GUTTER,
        ..fs.item_padding.into()
    }
}

/// Gap between the preview content and the pinned actions hint.
const PREVIEW_ACTIONS_HINT_SPACING: f32 = 6.0;
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

/// A small rounded label at the end of a row.
/// Opacity of rows drawn dimmed, such as muted Slack conversations.
const DIMMED_OPACITY: f32 = 0.45;

/// `title` led by a pin for pinned browser tabs.
fn pinned_title(title: &str, pinned: bool) -> String {
    if pinned {
        format!("📌 {title}")
    } else {
        title.to_string()
    }
}

fn badge_pill<'a>(label: String, t: ThemeColors, size: f32) -> Element<'a, Message> {
    tinted_badge_pill(label, t.accent, size)
}

fn tinted_badge_pill<'a>(label: String, accent: iced::Color, size: f32) -> Element<'a, Message> {
    container(text(label).size(size).color(accent))
        .padding([3.0, 10.0])
        .style(move |_theme| container::Style {
            background: Some(iced::Background::Color(iced::Color { a: 0.14, ..accent })),
            border: iced::Border {
                radius: 6.0.into(),
                ..Default::default()
            },
            ..Default::default()
        })
        .into()
}

/// Build an icon widget for `path`, or `None` when the file is missing.
fn icon_element<'a>(path: &Path, size: f32) -> Option<Element<'a, Message>> {
    faded_icon_element(path, size, 1.0)
}

fn faded_icon_element<'a>(path: &Path, size: f32, opacity: f32) -> Option<Element<'a, Message>> {
    path.exists()
        .then(|| existing_icon_element(path, size, opacity))
}

/// Like `faded_icon_element`, for a path already known to exist.
fn existing_icon_element<'a>(path: &Path, size: f32, opacity: f32) -> Element<'a, Message> {
    let is_svg = path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.eq_ignore_ascii_case("svg"))
        .unwrap_or(false);

    let thumbnail = is_svg.then(|| super::thumbnails::for_svg(path)).flatten();
    if is_svg && thumbnail.is_none() {
        svg(iced::widget::svg::Handle::from_path(path))
            .width(Length::Fixed(size))
            .height(Length::Fixed(size))
            .content_fit(iced::ContentFit::Contain)
            .opacity(opacity)
            .into()
    } else {
        image(thumbnail.unwrap_or_else(|| path.to_path_buf()))
            .width(Length::Fixed(size))
            .height(Length::Fixed(size))
            .content_fit(iced::ContentFit::Contain)
            .opacity(opacity)
            .into()
    }
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

fn mrkdwn_spans_to_iced<'a>(
    content: &str,
    ps: &PreviewStyle,
    base_color: iced::Color,
) -> Vec<HintSpan<'a>> {
    let bold_font = iced::Font {
        weight: iced::font::Weight::Bold,
        ..ps.font
    };
    let italic_font = iced::Font {
        style: iced::font::Style::Italic,
        ..ps.font
    };
    let medium_font = iced::Font {
        weight: iced::font::Weight::Medium,
        ..ps.font
    };
    parse_mrkdwn_spans(content)
        .into_iter()
        .map(|part| match part.style {
            MrkdwnStyle::Plain => span(part.text)
                .size(ps.text)
                .color(base_color)
                .font(ps.font),
            MrkdwnStyle::Bold => span(part.text)
                .size(ps.text)
                .color(base_color)
                .font(bold_font),
            MrkdwnStyle::Italic => span(part.text)
                .size(ps.text)
                .color(base_color)
                .font(italic_font),
            MrkdwnStyle::Strike => span(part.text)
                .size(ps.text)
                .color(ps.text_muted)
                .font(ps.font)
                .strikethrough(true),
            MrkdwnStyle::Code => span(format!(" {} ", part.text))
                .size(ps.small * 0.96)
                .color(ps.accent)
                .font(ps.mono_font)
                .background(ps.bg_block)
                .border(iced::Border {
                    color: iced::Color {
                        a: 0.45,
                        ..ps.border
                    },
                    width: 1.0,
                    radius: 4.0.into(),
                }),
            MrkdwnStyle::Mention => span(format!(" {} ", part.text))
                .size(ps.text)
                .color(ps.accent)
                .font(medium_font)
                .background(iced::Color {
                    a: 0.14,
                    ..ps.accent
                })
                .border(iced::Border {
                    radius: 4.0.into(),
                    ..Default::default()
                }),
            MrkdwnStyle::Link => span(part.text)
                .size(ps.text)
                .color(ps.accent)
                .font(ps.font)
                .underline(true),
        })
        .collect()
}

/// Container style shared by the chat feed's tinted blocks: a background and
/// hairline border derived from `background` and `border` with the given
/// alphas and corner radius.
fn tinted_block(
    background: iced::Color,
    background_alpha: f32,
    border: iced::Color,
    border_alpha: f32,
    radius: f32,
) -> impl Fn(&iced::Theme) -> container::Style {
    move |_theme| container::Style {
        background: Some(iced::Background::Color(iced::Color {
            a: background_alpha,
            ..background
        })),
        border: iced::Border {
            color: iced::Color {
                a: border_alpha,
                ..border
            },
            width: 1.0,
            radius: radius.into(),
        },
        ..Default::default()
    }
}

/// Horizontal hairline flanking a chat day-divider pill.
fn divider_line<'a>(color: iced::Color) -> Element<'a, Message> {
    container(Space::new())
        .width(Length::Fill)
        .height(Length::Fixed(1.0))
        .style(move |_theme| container::Style {
            background: Some(iced::Background::Color(iced::Color { a: 0.35, ..color })),
            ..Default::default()
        })
        .into()
}

fn render_chat_avatar<'a>(group: &ChatMessageGroup, ps: &PreviewStyle) -> Element<'a, Message> {
    const AVATAR_SIZE: f32 = 28.0;
    if let Some(path) = group.avatar.as_deref() {
        if let Some(img) = icon_element(Path::new(path), AVATAR_SIZE) {
            return container(img)
                .width(Length::Fixed(AVATAR_SIZE))
                .height(Length::Fixed(AVATAR_SIZE))
                .align_x(iced::Alignment::Center)
                .align_y(iced::Alignment::Center)
                .into();
        }
    }

    let initial = group
        .author
        .trim_start_matches(['@', '#'])
        .chars()
        .find(|c| c.is_alphanumeric())
        .or_else(|| group.author.chars().next())
        .unwrap_or('?')
        .to_uppercase()
        .to_string();
    let tint = if group.is_self {
        ps.accent
    } else {
        let hash = group.author.bytes().fold(0usize, |acc, b| {
            acc.wrapping_mul(31).wrapping_add(b as usize)
        });
        ps.ansi[1 + (hash % 6)]
    };
    let bold_font = iced::Font {
        weight: iced::font::Weight::Bold,
        ..ps.font
    };
    container(text(initial).size(ps.small).color(tint).font(bold_font))
        .width(Length::Fixed(AVATAR_SIZE))
        .height(Length::Fixed(AVATAR_SIZE))
        .align_x(iced::Alignment::Center)
        .align_y(iced::Alignment::Center)
        .style(tinted_block(tint, 0.18, tint, 0.38, 6.0))
        .into()
}

fn render_chat_feed<'a>(entries: Vec<ChatEntry>, ps: &PreviewStyle) -> Element<'a, Message> {
    if entries.is_empty() {
        return container(
            text("No messages")
                .size(ps.text)
                .color(ps.text_muted)
                .font(ps.font),
        )
        .padding(12.0)
        .width(Length::Fill)
        .style(tinted_block(
            ps.bg_block,
            ps.bg_block.a,
            ps.border,
            0.55,
            8.0,
        ))
        .into();
    }

    let bold_font = iced::Font {
        weight: iced::font::Weight::Bold,
        ..ps.font
    };
    let medium_font = iced::Font {
        weight: iced::font::Weight::Medium,
        ..ps.font
    };
    let mut feed = Column::new().spacing(12).width(Length::Fill);

    for entry in entries {
        match entry {
            ChatEntry::DayDivider(label) => {
                let pill = container(
                    text(label)
                        .size(ps.small * 0.92)
                        .color(ps.text_muted)
                        .font(medium_font),
                )
                .padding([2.0, 10.0])
                .style(tinted_block(
                    ps.bg_block,
                    ps.bg_block.a,
                    ps.border,
                    0.5,
                    10.0,
                ));
                feed = feed.push(
                    Row::new()
                        .spacing(8)
                        .align_y(iced::Alignment::Center)
                        .width(Length::Fill)
                        .push(divider_line(ps.border))
                        .push(pill)
                        .push(divider_line(ps.border)),
                );
            }
            ChatEntry::Messages(group) => {
                let avatar = render_chat_avatar(&group, ps);
                let author_color = if group.is_self {
                    ps.accent
                } else {
                    ps.text_main
                };
                let mut header = Row::new()
                    .spacing(8)
                    .align_y(iced::Alignment::Center)
                    .width(Length::Fill)
                    .push(
                        text(group.author)
                            .size(ps.text)
                            .color(author_color)
                            .font(bold_font),
                    );
                if !group.time.is_empty() {
                    header = header.push(
                        text(group.time)
                            .size(ps.small * 0.9)
                            .color(ps.text_muted)
                            .font(ps.font),
                    );
                }

                let mut body_col = Column::new().spacing(6).width(Length::Fill);
                for msg in group.messages {
                    let mut msg_col = Column::new().spacing(5).width(Length::Fill);
                    for block in parse_mrkdwn_blocks(&msg.text) {
                        match block {
                            MrkdwnBlock::Paragraph(para) => {
                                msg_col = msg_col.push(
                                    rich_text(mrkdwn_spans_to_iced(&para, ps, ps.text_main))
                                        .font(ps.font)
                                        .wrapping(iced::widget::text::Wrapping::WordOrGlyph),
                                );
                            }
                            MrkdwnBlock::Quote(quote) => {
                                msg_col = msg_col.push(
                                    container(
                                        rich_text(mrkdwn_spans_to_iced(&quote, ps, ps.text_muted))
                                            .font(ps.font)
                                            .wrapping(iced::widget::text::Wrapping::WordOrGlyph),
                                    )
                                    .padding([6.0, 10.0])
                                    .width(Length::Fill)
                                    .style(tinted_block(ps.accent, 0.07, ps.accent, 0.32, 6.0)),
                                );
                            }
                            MrkdwnBlock::Code(code) => {
                                msg_col = msg_col.push(
                                    container(
                                        text(code)
                                            .size(ps.small)
                                            .color(ps.text_main)
                                            .font(ps.mono_font)
                                            .wrapping(iced::widget::text::Wrapping::WordOrGlyph),
                                    )
                                    .padding([8.0, 10.0])
                                    .width(Length::Fill)
                                    .style(tinted_block(
                                        ps.bg_block,
                                        ps.bg_block.a,
                                        ps.border,
                                        0.55,
                                        6.0,
                                    )),
                                );
                            }
                        }
                    }

                    if !msg.reactions.is_empty() || msg.replies > 0 {
                        let mut pills = Row::new().spacing(6).align_y(iced::Alignment::Center);
                        for (emoji, count) in msg.reactions.into_iter().take(6) {
                            pills = pills.push(
                                container(
                                    text(format!("{emoji} {count}"))
                                        .size(ps.small * 0.9)
                                        .color(ps.text_main)
                                        .font(ps.font),
                                )
                                .padding([2.0, 7.0])
                                .style(tinted_block(
                                    ps.bg_block,
                                    ps.bg_block.a,
                                    ps.border,
                                    0.5,
                                    10.0,
                                )),
                            );
                        }
                        if msg.replies > 0 {
                            let accent = ps.accent;
                            let label = if msg.replies == 1 {
                                "💬 1 reply →".to_string()
                            } else {
                                format!("💬 {} replies →", msg.replies)
                            };
                            pills = pills.push(
                                container(
                                    text(label)
                                        .size(ps.small * 0.9)
                                        .color(accent)
                                        .font(medium_font),
                                )
                                .padding([2.0, 8.0])
                                .style(tinted_block(accent, 0.12, accent, 0.35, 10.0)),
                            );
                        }
                        msg_col = msg_col.push(pills);
                    }

                    if msg.truncated {
                        msg_col = msg_col.push(
                            text("Message truncated…")
                                .size(ps.small * 0.9)
                                .color(ps.text_muted)
                                .font(ps.font),
                        );
                    }

                    let msg_box: Element<'_, Message> = if msg.highlighted {
                        container(msg_col)
                            .padding([6.0, 8.0])
                            .width(Length::Fill)
                            .style(tinted_block(ps.accent, 0.11, ps.accent, 0.42, 6.0))
                            .into()
                    } else {
                        msg_col.into()
                    };

                    if msg.desktop_url.is_some() || msg.browser_url.is_some() {
                        body_col = body_col.push(
                            mouse_area(msg_box)
                                .on_release(Message::SlackPreviewOpen {
                                    desktop_url: msg.desktop_url,
                                    browser_url: msg.browser_url,
                                })
                                .interaction(iced::mouse::Interaction::Pointer),
                        );
                    } else {
                        body_col = body_col.push(msg_box);
                    }
                }

                let right_col = Column::new()
                    .spacing(3)
                    .width(Length::Fill)
                    .push(header)
                    .push(body_col);

                feed = feed.push(
                    Row::new()
                        .spacing(10)
                        .align_y(iced::Alignment::Start)
                        .width(Length::Fill)
                        .push(avatar)
                        .push(right_col),
                );
            }
        }
    }

    feed.into()
}

/// The icon, title, subtitle and badge row at the top of the preview pane.
fn preview_header<'a>(
    ps: &PreviewStyle,
    icon: Option<PreviewIcon>,
    title: String,
    subtitle: Option<String>,
    badge: Option<String>,
) -> Row<'a, Message> {
    let mut header_row = Row::new()
        .spacing(14)
        .align_y(iced::Alignment::Center)
        .width(Length::Fill);

    if let Some(icon) = icon {
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
    if !title.is_empty() {
        title_col = title_col.push(text(title).size(ps.title).color(ps.text_main).font(ps.font));
    }
    if let Some(sub) = subtitle {
        title_col = title_col.push(text(sub).size(ps.small).color(ps.text_muted).font(ps.font));
    }
    header_row = header_row.push(title_col);

    if let Some(badge) = badge {
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
    header_row
}

impl LauncherApp {
    /// Wrap the shared window chrome around an already built item list.
    pub(super) fn shell<'a>(
        &'a self,
        placeholder: &'a str,
        hint_spans: Vec<HintSpan<'a>>,
        items_column: Column<'a, Message>,
    ) -> Element<'a, Message> {
        let t = self.view.theme;
        let fs = self.view.font_sizes;

        // The master password or passphrase prompt reuses the search field,
        // masked, and its text never goes through `SearchChanged`.
        let prompt = self.vault.prompt.as_ref();
        let search = self
            .bookmarks
            .prompt
            .as_ref()
            .filter(|_| !self.view.help_active);
        let (placeholder, value) = match (prompt, search) {
            (Some(prompt), _) => (prompt.purpose.placeholder(), prompt.buffer.expose()),
            (None, Some(search)) => (search.placeholder.as_str(), search.term.as_str()),
            (None, None) => (placeholder, self.search_query.as_str()),
        };
        let save = self.snippet_save.as_ref().filter(|_| self.clipboard.active);
        let (placeholder, value) = match save {
            Some(save) => (save.placeholder(), save.buffer.as_str()),
            None => (placeholder, value),
        };
        let (placeholder, value) = self
            .action_menu
            .as_ref()
            .map(|menu| ("Search actions...", menu.query.as_str()))
            .unwrap_or((placeholder, value));
        let secure = prompt.is_some();
        // Normal mode takes the keyboard: a disabled input cannot insert typed
        // characters, and focus is removed by the vim mode handlers.
        let vim_normal = self.view.vim.is_normal()
            && prompt.is_none()
            && search.is_none()
            && save.is_none()
            && self.action_menu.is_none();
        let mut search_input = text_input(placeholder, value)
            .id(self.view.search_input_id.clone())
            .secure(secure)
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
        if !vim_normal {
            // Enter is handled by the subscription alone, which knows about
            // key autorepeat; an `on_submit` here would deliver it twice.
            search_input = search_input.on_input(move |value| {
                if secure {
                    Message::PromptChanged(crate::secret::Secret::new(value))
                } else {
                    Message::SearchChanged(value)
                }
            });
        }

        let search_input: Element<'a, Message> = Element::new(super::action_input::ActionInput {
            content: search_input.into(),
            value,
            enabled: self.action_menu.is_none() && !self.selected_actions().is_empty(),
        });

        let search_control: Element<'a, Message> = if self.view.vim.enabled {
            let (label, background, foreground) = if vim_normal {
                ("NORMAL", t.accent, t.bg_input)
            } else {
                ("INSERT", t.bg_input, t.text_muted)
            };
            let badge = container(text(label).size(fs.hint).color(foreground))
                .padding([4, 8])
                .style(move |_theme| container::Style {
                    background: Some(iced::Background::Color(background)),
                    border: iced::Border {
                        color: t.border,
                        width: 1.0,
                        radius: 8.0.into(),
                    },
                    ..Default::default()
                });
            Row::new()
                .spacing(8)
                .align_y(iced::Alignment::Center)
                .push(badge)
                .push(container(search_input).width(Length::Fill))
                .into()
        } else {
            search_input
        };

        let items_container = container(items_column)
            .id(self.view.items_container_id.clone())
            .width(Length::Fill)
            .height(Length::Shrink);

        let items_scroll = scrollable(items_container)
            .id(self.view.scrollable_id.clone())
            .direction(slim_scrollbar())
            .style(scrollable_style(t))
            .height(Length::Fill)
            .width(Length::Fill)
            .on_scroll(|viewport| Message::ListScrolled(viewport.relative_offset().y));

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

        let hint_spans = if self.action_menu.is_some() {
            self.action_menu_hints()
        } else {
            hint_spans
        };
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

        let preview = self
            .resolve_selected_preview()
            .filter(|_| self.onboarding.setup.is_none());
        let body_area: Element<'a, Message> = if preview.is_some() || self.keeps_empty_preview() {
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

            let pane = match (&self.action_menu, preview) {
                (Some(menu), Some(data)) => self.render_action_pane(menu, data),
                (None, Some(data)) => self.render_preview_pane(data),
                (_, None) => self.preview_frame(
                    Space::new().width(Length::Fill).height(Length::Fill),
                    self.view.preview_style.bg,
                ),
            };
            let right_pane = container(pane)
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

        let mut content = Column::new()
            .spacing(10)
            .width(Length::Fill)
            .height(Length::Fill)
            .push(search_control);
        if self.welcome_visible() {
            content = content.push(self.render_welcome());
        }
        let content = content.push(body_area).push(footer);

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
    /// Icon file of a script filter row, else the filter's default icon.
    fn script_filter_icon(
        &self,
        item: &ScriptFilterItem,
        result: &ScriptFilterResult,
    ) -> Option<String> {
        item.icon
            .as_ref()
            .and_then(|icon| icon.path.clone())
            .map(|path| crate::expand_config_value(&path))
            .filter(|path| Path::new(path).exists())
            .or_else(|| {
                result
                    .default_icon
                    .as_ref()
                    .and_then(|name| self.icon_map.get(name).cloned())
            })
    }

    /// The heading over a section of command list rows, such as a browser.
    fn row_group_header(&self, group: &super::command_list::RowGroup) -> Element<'_, Message> {
        let t = self.view.theme;
        let fs = self.view.font_sizes;
        let mut header = Row::new().spacing(8).align_y(iced::Alignment::Center);
        if let Some(icon) = group
            .icon
            .as_deref()
            .and_then(|path| icon_element(Path::new(path), 20.0))
        {
            header = header.push(icon);
        }
        header = header.push(text(group.title.clone()).size(fs.subtitle).color(t.accent));
        container(header)
            .padding(iced::Padding {
                top: 6.0,
                bottom: 0.0,
                left: fs.item_padding,
                right: fs.item_padding,
            })
            .into()
    }

    /// Row `row` of main view source `source`, shown at `display_idx`, with
    /// a badge naming where it comes from.
    fn main_view_row(
        &self,
        source: usize,
        row: usize,
        display_idx: usize,
        guides: &[Vec<TreeGuideSegments>],
    ) -> Option<Element<'_, Message>> {
        let t = self.view.theme;
        let fs = self.view.font_sizes;
        let mut item_row = Row::new().spacing(16).align_y(iced::Alignment::Center);
        let mut text_col = Column::new();
        let mut badges = Vec::new();
        match &self.main_view.sources[source] {
            MainViewSource::List(list) => {
                let item = list.item(row)?;
                if let Some(icon) = item
                    .icon
                    .as_deref()
                    .and_then(|path| icon_element(Path::new(path), 40.0))
                {
                    item_row = item_row.push(icon);
                }
                text_col = text_col
                    .push(
                        text(pinned_title(&item.title, item.pinned))
                            .size(fs.item)
                            .color(t.text_main),
                    )
                    .push(
                        text(item.subtitle.clone())
                            .size(fs.subtitle)
                            .color(t.text_muted),
                    );
                badges.extend(item.badge.clone());
                badges.extend(list.addon.map(|addon| addon.badge().to_string()));
            }
            MainViewSource::Snippets {
                config_index,
                items,
                filtered,
                previews,
                ..
            } => {
                let item = *filtered.get(row)?;
                let snippet = items.get(item)?;
                let config = self.addons.text_snippets.get(*config_index)?;
                let icon = snippet
                    .icon
                    .iter()
                    .chain(config.icon.iter())
                    .find_map(|icon| self.resolve_icon_or_glyph(icon));
                match icon {
                    Some(SnippetIcon::File(path)) => {
                        if let Some(icon) = icon_element(Path::new(&path), 40.0) {
                            item_row = item_row.push(icon);
                        }
                    }
                    Some(SnippetIcon::Glyph(glyph)) => {
                        item_row = item_row
                            .push(text(glyph).size(fs.item + 4.0).width(Length::Fixed(40.0)));
                    }
                    None => {}
                }
                text_col = text_col
                    .push(text(&snippet.name).size(fs.item).color(t.text_main))
                    .push(
                        text(truncate_preview(
                            previews
                                .get(item)
                                .and_then(Option::as_deref)
                                .unwrap_or(&snippet.value),
                        ))
                        .size(fs.subtitle)
                        .color(t.text_muted),
                    );
                badges.push(config.name.clone());
            }
            MainViewSource::ScriptFilter {
                config_index,
                results,
                ..
            } => {
                let result = results.as_ref()?;
                let item = result.items.get(row)?;
                if let Some(icon) = self
                    .script_filter_icon(item, result)
                    .and_then(|path| icon_element(Path::new(&path), 40.0))
                {
                    item_row = item_row.push(icon);
                }
                text_col = text_col.push(rich_text(ansi_to_spans(
                    &item.title,
                    fs.item,
                    t.text_main,
                    iced::Font::default(),
                    &self.view.preview_style.ansi,
                )));
                if let Some(subtitle) = &item.subtitle {
                    text_col = text_col.push(rich_text(ansi_to_spans(
                        subtitle,
                        fs.subtitle,
                        t.text_muted,
                        iced::Font::default(),
                        &self.view.preview_style.ansi,
                    )));
                }
                badges.extend(
                    self.addons
                        .script_filters
                        .get(*config_index)
                        .map(|config| config.name.clone()),
                );
            }
        }
        item_row = item_row.push(text_col.width(Length::Fill));
        for badge in badges {
            item_row = item_row.push(badge_pill(badge, t, fs.subtitle));
        }
        let indent = match &self.main_view.sources[source] {
            MainViewSource::ScriptFilter { results, .. } => {
                let result = results.as_ref()?;
                indent_left(result.items.get(row)?.indent)
            }
            _ => 0.0,
        };
        let is_selected = display_idx == self.selected_index;
        let item_button = button(item_row)
            .on_press(Message::ItemClicked(display_idx))
            .padding(iced::Padding {
                left: fs.item_padding + indent,
                ..row_padding(fs)
            })
            .width(Length::Fill)
            .style(item_button_style(t, is_selected, t.text_main));
        Some(self.selectable(
            with_tree_guide(
                item_button,
                guides.get(row).cloned().unwrap_or_default(),
                fs.item_padding,
                t,
            ),
            display_idx,
        ))
    }

    /// Wraps the selected row in the container `snap_to_selection` measures
    /// to scroll it into view. Containers share their content's widget tree,
    /// so moving the wrapper between rows keeps their state.
    /// A result row at `index`: right-click opens its actions, and the
    /// selected row gets the actions chevron and the scroll tag.
    fn selectable<'a>(
        &self,
        row: impl Into<Element<'a, Message>>,
        index: usize,
    ) -> Element<'a, Message> {
        let is_selected = index == self.selected_index;
        let row: Element<'a, Message> = if self.view.help_active {
            row.into()
        } else {
            mouse_area(row)
                .on_right_press(Message::OpenActionsAt(index))
                .into()
        };
        let row = if is_selected {
            self.with_action_chevron(row)
        } else {
            row
        };
        self.tag_selected(row, is_selected)
    }

    /// Overlays a `›` on the right of the selected row when it has actions,
    /// highlighted while they are open. Clicking it toggles them.
    fn with_action_chevron<'a>(&self, row: Element<'a, Message>) -> Element<'a, Message> {
        let t = self.view.theme;
        let fs = self.view.font_sizes;
        let (color, message) = if self.action_menu.is_some() {
            (t.accent, Message::Cancel)
        } else if !self.view.help_active && !self.selected_actions().is_empty() {
            (t.text_muted, Message::OpenActions)
        } else {
            return row;
        };
        let chevron = button(
            text("›")
                .size(fs.item + 6.0)
                .color(color)
                .width(Length::Fill)
                .align_x(iced::alignment::Horizontal::Center),
        )
        .on_press(message)
        .padding(0)
        .width(Length::Fixed(ACTION_CHEVRON_GUTTER + fs.item_padding / 2.0))
        .style(|_, _| button::Style::default());
        // Result rows keep `ACTION_CHEVRON_GUTTER` free on their right, so the
        // chevron never covers a badge or a time.
        iced::widget::stack![
            row,
            container(chevron)
                .width(Length::Fill)
                .height(Length::Fill)
                .align_x(iced::alignment::Horizontal::Right)
                .align_y(iced::alignment::Vertical::Center)
                .padding(iced::Padding {
                    right: fs.item_padding / 2.0,
                    ..Default::default()
                }),
        ]
        .into()
    }

    /// Tags the selected row so `snap_to_selection` can measure it.
    fn tag_selected<'a>(
        &self,
        row: impl Into<Element<'a, Message>>,
        is_selected: bool,
    ) -> Element<'a, Message> {
        if is_selected {
            container(row)
                .id(self.view.selected_row_id.clone())
                .width(Length::Fill)
                .into()
        } else {
            row.into()
        }
    }

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

            let icon_slot: Element<Message> = match entry
                .icon
                .as_deref()
                .and_then(|icon| self.keyword_icon(icon))
            {
                Some(SnippetIcon::File(path)) => icon_element(Path::new(&path), icon_size),
                Some(SnippetIcon::Glyph(glyph)) => Some(
                    container(text(glyph).size(fs.item + 4.0))
                        .center(Length::Fixed(icon_size))
                        .into(),
                ),
                None => None,
            }
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

            items_column = items_column.push(self.selectable(item_button, display_idx));
        }

        items_column
    }

    fn action_menu_hints(&self) -> Vec<HintSpan<'_>> {
        let t = self.view.theme;
        let fs = self.view.font_sizes;
        vec![
            span("Enter").size(fs.hint).color(t.accent),
            span(" run action").size(fs.hint).color(t.text_muted),
            span("  ·  ").size(fs.hint).color(t.border),
            span("Esc/←").size(fs.hint).color(t.accent),
            span(" back to results").size(fs.hint).color(t.text_muted),
        ]
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
        let hint = if self.preview_action_count() > 0 {
            self.view.preview_style.small * 1.3 + 4.0 + PREVIEW_ACTIONS_HINT_SPACING
        } else {
            0.0
        };
        (self.view.window_size.height - chrome - hint).max(160.0)
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
        let body_text = |value: String, font: iced::Font| {
            text(value)
                .size(ps.text)
                .color(ps.text_main)
                .font(font)
                .wrapping(iced::widget::text::Wrapping::WordOrGlyph)
        };

        if let Some(PreviewBody::Plain { .. }) = data.bodies.first() {
            return self.render_plain_preview(data, ps);
        }

        let mut col = Column::new().spacing(14).width(Length::Fill);

        // Image previews fill the whole pane: the image is rendered first at the
        // full viewport size, and the header plus file info follow below the fold,
        // reachable by scrolling with Ctrl+J / Ctrl+K.
        let image_first = matches!(
            data.bodies.first(),
            Some(PreviewBody::Image { .. } | PreviewBody::ImageData { .. })
        );
        let viewport_height = self.preview_viewport_height();

        let header_row = preview_header(&ps, data.icon, data.title, data.subtitle, data.badge);

        if image_first {
            match data.bodies.first() {
                Some(PreviewBody::Image { path }) => {
                    if let Some(el) = full_pane_image(path, viewport_height) {
                        col = col.push(el);
                    }
                }
                Some(PreviewBody::ImageData {
                    handle,
                    footer: None,
                }) => {
                    col = col.push(memory_image(handle.clone(), viewport_height));
                }
                // The summary line stays in the first view, under the image.
                Some(PreviewBody::ImageData {
                    handle,
                    footer: Some(footer),
                }) => {
                    col = col.push(memory_image(
                        handle.clone(),
                        (viewport_height - ps.small * 1.5 - 14.0).max(0.0),
                    ));
                    col = col.push(
                        container(
                            text(footer.clone())
                                .size(ps.small)
                                .color(ps.text_muted)
                                .font(ps.font),
                        )
                        .width(Length::Fill)
                        .align_x(iced::Alignment::Center),
                    );
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
            let prose = matches!(body, PreviewBody::Prose { .. });
            match body {
                PreviewBody::Text {
                    heading: h,
                    content,
                    truncated,
                }
                | PreviewBody::Prose {
                    heading: h,
                    content,
                    truncated,
                } => {
                    let font = if prose { ps.font } else { ps.mono_font };
                    let mut section = Column::new().spacing(6).width(Length::Fill);
                    if let Some(h) = h {
                        section = section.push(heading(h));
                    }
                    section = section.push(
                        container(body_text(content, font))
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
                // Only rendered as the sole body, by `render_plain_preview`.
                PreviewBody::Plain { .. } => {}
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
                            rich_text(ansi_to_spans(
                                &content,
                                ps.text,
                                ps.text_main,
                                ps.mono_font,
                                &ps.ansi,
                            ))
                            .font(ps.mono_font),
                        )
                        .padding(12.0)
                        .width(Length::Fill)
                        .style(block_style),
                    );
                    col = col.push(section);
                }
                PreviewBody::Chat {
                    heading: h,
                    entries,
                } => {
                    let mut section = Column::new().spacing(10).width(Length::Fill);
                    if let Some(h) = h {
                        section = section.push(heading(h));
                    }
                    section = section.push(render_chat_feed(entries, &ps));
                    col = col.push(section);
                }
                PreviewBody::Image { path } => {
                    if let Some(el) = full_pane_image(&path, viewport_height) {
                        col = col.push(el);
                    }
                }
                PreviewBody::ImageData { handle, .. } => {
                    col = col.push(memory_image(handle, viewport_height));
                }
                PreviewBody::Thumbnail { path } => {
                    col = col.push(
                        container(
                            image(PathBuf::from(path))
                                .width(Length::Fill)
                                .content_fit(iced::ContentFit::Contain),
                        )
                        .max_height(viewport_height / 2.0),
                    );
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

        self.preview_frame(scroll, ps.bg)
    }

    /// The actions for the selected result, listed in the preview pane under
    /// the result's header while the action menu is open.
    fn render_action_pane<'a>(
        &'a self,
        menu: &'a super::action_menu::ActionMenu,
        data: PreviewData,
    ) -> Element<'a, Message> {
        let t = self.view.theme;
        let fs = self.view.font_sizes;
        let ps = self.view.preview_style.clone();
        let border = ps.border;

        let mut col = Column::new()
            .spacing(14)
            .width(Length::Fill)
            .push(preview_header(
                &ps,
                data.icon,
                data.title,
                data.subtitle,
                data.badge,
            ))
            .push(
                container(Space::new())
                    .width(Length::Fill)
                    .height(Length::Fixed(1.0))
                    .style(move |_theme| container::Style {
                        background: Some(iced::Background::Color(iced::Color { a: 0.3, ..border })),
                        ..Default::default()
                    }),
            )
            .push(
                text("Actions")
                    .size(ps.small)
                    .color(ps.accent)
                    .font(ps.font),
            );

        let filtered = menu.filtered();
        let mut rows = Column::new().spacing(ITEM_SPACING).width(Length::Fill);
        if filtered.is_empty() {
            rows = rows.push(
                text("No matching actions. Clear the filter to see all actions.")
                    .size(ps.text)
                    .color(ps.text_muted)
                    .font(ps.font),
            );
        }
        for (row, index) in filtered.into_iter().enumerate() {
            let entry = &menu.entries[index];
            let selected = row == menu.selected;
            let mut line = Row::new().spacing(12).align_y(iced::Alignment::Center);
            if let Some(glyph) = entry.glyph {
                line = line.push(
                    text(glyph)
                        .size(ps.text)
                        .width(Length::Fixed(ps.text + 8.0))
                        .align_x(iced::alignment::Horizontal::Center),
                );
            }
            line = line.push(
                text(&entry.label)
                    .size(ps.text)
                    .font(ps.font)
                    .width(Length::Fill)
                    .wrapping(iced::widget::text::Wrapping::WordOrGlyph),
            );
            if !entry.shortcut.is_empty() {
                line = line.push(
                    text(&entry.shortcut)
                        .size(ps.small)
                        .color(t.text_muted)
                        .font(ps.font),
                );
            }
            let action = button(line)
                .on_press(Message::ActionSelected(index))
                .padding(fs.item_padding)
                .width(Length::Fill)
                .style(item_button_style(t, selected, ps.text_main));
            rows = rows.push(if selected {
                Element::from(
                    container(action)
                        .id(self.view.action_row_id.clone())
                        .width(Length::Fill),
                )
            } else {
                action.into()
            });
        }
        col = col.push(rows);

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

        self.preview_frame(scroll, ps.bg)
    }

    /// Bare text with a centred summary line, no title, badge, box or metadata.
    fn render_plain_preview(&self, data: PreviewData, ps: PreviewStyle) -> Element<'_, Message> {
        let t = self.view.theme;
        let Some(PreviewBody::Plain { content, footer }) = data.bodies.into_iter().next() else {
            return Space::new().into();
        };
        let body = scrollable(
            container(
                text(content)
                    .size(ps.text)
                    .color(ps.text_main)
                    .font(ps.font)
                    .wrapping(iced::widget::text::Wrapping::WordOrGlyph)
                    .width(Length::Fill),
            )
            .padding(iced::Padding {
                top: 0.0,
                right: PREVIEW_CONTENT_RIGHT_PADDING,
                bottom: 0.0,
                left: 0.0,
            }),
        )
        .id(self.view.preview_scrollable_id.clone())
        .direction(slim_scrollbar())
        .style(scrollable_style(t))
        .width(Length::Fill)
        .height(Length::Fill);

        let mut col = Column::new().spacing(8).width(Length::Fill).push(body);
        if let Some(footer) = footer {
            col = col.push(
                container(
                    text(footer)
                        .size(ps.small)
                        .color(ps.text_muted)
                        .font(ps.font),
                )
                .width(Length::Fill)
                .align_x(iced::Alignment::Center),
            );
        }

        self.preview_frame(col, ps.bg)
    }

    /// The number of actions the preview pane advertises for the selected
    /// result, or zero while the menu is open or there are none.
    fn preview_action_count(&self) -> usize {
        if self.action_menu.is_some() || self.view.help_active {
            0
        } else {
            self.selected_actions().len()
        }
    }

    /// The rounded, padded background shared by the preview pane contents,
    /// with a pinned line pointing at the result's actions.
    /// The main page keeps the preview pane open on rows with nothing to
    /// preview, so the list does not jump between widths while moving.
    pub(super) fn keeps_empty_preview(&self) -> bool {
        self.main_view.active
            && self.view.show_preview
            && !self.view.help_active
            && self.onboarding.setup.is_none()
    }

    fn preview_frame<'a>(
        &self,
        content: impl Into<Element<'a, Message>>,
        bg: iced::Color,
    ) -> Element<'a, Message> {
        let ps = &self.view.preview_style;
        let count = self.preview_action_count();
        let body: Element<'a, Message> = if count == 0 {
            content.into()
        } else {
            let accent = ps.accent;
            let label = if count == 1 {
                "1 action".to_string()
            } else {
                format!("{count} actions")
            };
            let hint = button(
                Row::new()
                    .spacing(8)
                    .align_y(iced::Alignment::Center)
                    .push(
                        text(label)
                            .size(ps.small)
                            .color(ps.text_muted)
                            .font(ps.font),
                    )
                    .push(text("→").size(ps.small).color(accent).font(ps.font)),
            )
            .on_press(Message::OpenActions)
            .padding([2.0, 6.0])
            .style(move |_, status| button::Style {
                background: (status == button::Status::Hovered)
                    .then(|| iced::Color { a: 0.14, ..accent }.into()),
                border: iced::Border {
                    radius: 6.0.into(),
                    ..Default::default()
                },
                ..Default::default()
            });
            Column::new()
                .spacing(PREVIEW_ACTIONS_HINT_SPACING)
                .width(Length::Fill)
                .height(Length::Fill)
                .push(container(content).width(Length::Fill).height(Length::Fill))
                .push(
                    container(hint)
                        .width(Length::Fill)
                        .align_x(iced::alignment::Horizontal::Right),
                )
                .into()
        };
        container(body)
            .padding(iced::Padding {
                top: 8.0,
                right: PREVIEW_PANE_RIGHT_PADDING,
                bottom: 8.0,
                left: PREVIEW_PANE_LEFT_PADDING,
            })
            .width(Length::Fill)
            .height(Length::Fill)
            .style(move |_theme| container::Style {
                background: Some(iced::Background::Color(bg)),
                border: iced::Border {
                    radius: 10.0.into(),
                    ..Default::default()
                },
                ..Default::default()
            })
            .into()
    }

    pub(super) fn empty_search_enter_hint(&self) -> Option<String> {
        if self.welcome_visible() {
            if let Some(action) = self
                .onboarding
                .focused
                .and_then(|row| self.welcome_actions().get(row).copied())
            {
                return Some(action.label().into());
            }
        }
        if !self.search_query.is_empty() || self.view.help_active {
            return None;
        }
        if self.keyword_chips && self.selected_index == 0 {
            let chip = self.keyword_suggestions.get(self.chip_index)?;
            return Some(format!(
                "Open {}",
                super::main_view::chip_label(&chip.keyword, &self.addons)
            ));
        }
        match self.selected_main_row()? {
            MainRowRef::More(_) => None,
            MainRowRef::Launcher(index) => {
                let config = self.configs.get(index)?;
                Some(format!(
                    "Launch {}",
                    config.description.as_deref().or(config.binary.as_deref())?
                ))
            }
            MainRowRef::Source { source, row } => match self.main_view.sources.get(source)? {
                MainViewSource::List(list) => {
                    let item = list.item(row)?;
                    if item.unavailable.is_some() || item.primary.is_none() {
                        return None;
                    }
                    item.details
                        .iter()
                        .find(|(key, _)| key == "Enter")
                        .map(|(_, label)| label.clone())
                }
                _ => self
                    .resolve_selected_preview()?
                    .metadata
                    .into_iter()
                    .find(|(key, _)| key == "Enter")
                    .map(|(_, label)| label),
            },
        }
    }

    /// What each chip shows, per `general.main_view_chips`.
    pub(super) fn chip_labels(&self) -> Vec<String> {
        self.keyword_suggestions
            .iter()
            .map(|suggestion| match self.main_view.chips {
                crate::MainViewChips::Keyword => suggestion.keyword.clone(),
                _ => super::main_view::chip_label(&suggestion.keyword, &self.addons),
            })
            .collect()
    }

    /// The plugins offered on an empty query, as a single row: icon and label
    /// per chip, the active one in a pill, "‹" / "›" where chips are hidden.
    fn chips_row(&self) -> Element<'_, Message> {
        let t = self.view.theme;
        let fs = self.view.font_sizes;
        let row_selected = self.selected_index == 0;
        let icon_size = (fs.item + 2.0).round();
        let label_size = fs.subtitle + 1.0;
        let icons: Vec<_> = self
            .keyword_suggestions
            .iter()
            .map(|suggestion| {
                suggestion
                    .icon
                    .as_deref()
                    .and_then(|icon| self.keyword_icon(icon))
            })
            .collect();
        let labels = self.chip_labels();
        let widths: Vec<f32> = labels
            .iter()
            .zip(&icons)
            .map(|(label, icon)| chip_width(label, icon.as_ref().map(|_| icon_size), label_size))
            .collect();
        let active = self
            .chip_index
            .min(self.keyword_suggestions.len().saturating_sub(1));
        let subtitle = (row_selected && self.resolve_selected_preview().is_none()).then(|| {
            let suggestion = &self.keyword_suggestions[active];
            if labels[active] == suggestion.keyword {
                suggestion.subtitle.clone()
            } else {
                format!("{} · {}", suggestion.keyword, suggestion.subtitle)
            }
        });

        let strip = iced::widget::responsive(move |size| {
            let window = chip_window(&widths, size.width, active);
            let marker = |glyph: &'static str, backwards: bool| {
                button(
                    container(text(glyph).size(fs.item).color(t.text_muted))
                        .center_x(Length::Fixed(CHIP_MARKER_WIDTH)),
                )
                .on_press(Message::ChipScroll(backwards))
                .padding(0)
                .style(chip_style(t, false, row_selected))
            };
            let mut chips = Row::new()
                .spacing(CHIP_SPACING)
                .align_y(iced::Alignment::Center);
            if window.start > 0 {
                chips = chips.push(marker("‹", true));
            }
            for chip in window.clone() {
                let is_active = chip == active;
                let mut content = Row::new()
                    .spacing(CHIP_ICON_SPACING)
                    .align_y(iced::Alignment::Center);
                match &icons[chip] {
                    Some(SnippetIcon::File(path)) => {
                        if let Some(icon) = icon_element(Path::new(path), icon_size) {
                            content = content.push(icon);
                        }
                    }
                    Some(SnippetIcon::Glyph(glyph)) => {
                        content = content.push(
                            container(text(glyph.clone()).size(fs.item))
                                .center(Length::Fixed(icon_size)),
                        );
                    }
                    None => {}
                }
                content = content.push(
                    text(labels[chip].clone())
                        .size(label_size)
                        .color(if is_active { t.accent } else { t.text_muted }),
                );
                chips = chips.push(
                    button(content)
                        .on_press(Message::ChipClicked(chip))
                        .padding([4.0, CHIP_PADDING_X])
                        .style(chip_style(t, is_active, row_selected)),
                );
            }
            if window.end < widths.len() {
                chips = chips.push(marker("›", false));
            }
            let mut line = Row::new()
                .spacing(16)
                .height(Length::Fill)
                .align_y(iced::Alignment::Center)
                .push(chips);
            // What is left after the chips, in subtitle-sized characters.
            let room = ((size.width - chips_width(&widths, window.clone()) - 16.0)
                / (fs.subtitle * CHIP_GLYPH_EM))
                .max(0.0) as usize;
            let subtitle = subtitle.as_deref().and_then(|subtitle| {
                if subtitle.chars().count() <= room {
                    Some(subtitle.to_string())
                } else if room >= 12 {
                    let cut: String = subtitle.chars().take(room - 1).collect();
                    Some(format!("{}…", cut.trim_end()))
                } else {
                    None
                }
            });
            if let Some(subtitle) = subtitle {
                line = line.push(
                    text(subtitle)
                        .size(fs.subtitle)
                        .color(t.text_muted)
                        .wrapping(iced::widget::text::Wrapping::None),
                );
            }
            container(line).clip(true).into()
        })
        .height(Length::Fixed(40.0));

        container(strip)
            .padding(fs.item_padding)
            .width(Length::Fill)
            .style(item_row_style(t, row_selected, t.text_main))
            .into()
    }

    pub(super) fn view(&self) -> Element<'_, Message> {
        static FIRST_VIEW: std::sync::Once = std::sync::Once::new();
        FIRST_VIEW.call_once(|| crate::debug::mark("first view"));
        if self.onboarding.setup.is_some() {
            return self.render_onboarding_setup();
        }
        let t = self.view.theme;
        let fs = self.view.font_sizes;

        // With the preview pane shown, the actions take it over and the
        // results stay on the left; without it they fill the window.
        if let Some(menu) = self
            .action_menu
            .as_ref()
            .filter(|_| self.resolve_selected_preview().is_none())
        {
            let mut items = Column::new().spacing(ITEM_SPACING).push(
                text(format!("Actions for {}", menu.title))
                    .size(fs.subtitle)
                    .color(t.text_muted),
            );
            let filtered = menu.filtered();
            if filtered.is_empty() {
                items = items.push(
                    text("No matching actions. Clear the filter to see all actions.")
                        .size(fs.item)
                        .color(t.text_muted),
                );
            }
            for (row, index) in filtered.into_iter().enumerate() {
                let entry = &menu.entries[index];
                let mut action_row = Row::new().spacing(12).align_y(iced::Alignment::Center);
                if let Some(glyph) = entry.glyph {
                    action_row = action_row.push(
                        text(glyph)
                            .size(fs.item)
                            .width(Length::Fixed(fs.item + 8.0))
                            .align_x(iced::alignment::Horizontal::Center),
                    );
                }
                items = items.push(
                    button(
                        action_row
                            .push(text(&entry.label).size(fs.item).width(Length::Fill))
                            .push(text(&entry.shortcut).size(fs.hint).color(t.text_muted)),
                    )
                    .on_press(Message::ActionSelected(index))
                    .padding(fs.item_padding)
                    .width(Length::Fill)
                    .style(item_button_style(
                        t,
                        row == menu.selected,
                        t.text_main,
                    )),
                );
            }
            return self.shell("Search actions...", self.action_menu_hints(), items);
        }

        if self.view.help_active {
            return self.shell(
                "Search help...",
                self.help_hint_spans(),
                self.help_items_column(),
            );
        }

        let mut items_column = Column::new().spacing(ITEM_SPACING);
        let main_view_guides: Vec<_> = self
            .main_view
            .sources
            .iter()
            .enumerate()
            .map(|(source, entry)| match entry {
                MainViewSource::ScriptFilter {
                    results: Some(result),
                    ..
                } => tree_guide_segments(&result.items[..self.main_view.source_rows(source)]),
                _ => Vec::new(),
            })
            .collect();

        let mut special_item_idx = 0;

        if self.keyword_chips && !self.keyword_suggestions.is_empty() {
            items_column =
                items_column.push(self.tag_selected(self.chips_row(), self.selected_index == 0));
            special_item_idx += 1;
        }

        for suggestion in self
            .keyword_suggestions
            .iter()
            .filter(|_| !self.keyword_chips)
        {
            let display_idx = special_item_idx;
            let is_selected = display_idx == self.selected_index;
            let mut item_row = Row::new().spacing(16).align_y(iced::Alignment::Center);

            match suggestion
                .icon
                .as_deref()
                .and_then(|icon| self.keyword_icon(icon))
            {
                Some(SnippetIcon::File(path)) => {
                    if let Some(icon) = icon_element(Path::new(&path), 40.0) {
                        item_row = item_row.push(icon);
                    }
                }
                Some(SnippetIcon::Glyph(glyph)) => {
                    item_row = item_row.push(
                        container(text(glyph).size(fs.item + 4.0)).center(Length::Fixed(40.0)),
                    );
                }
                None => {}
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
                .padding(row_padding(fs))
                .width(Length::Fill)
                .style(item_button_style(t, is_selected, t.text_main));

            items_column = items_column.push(self.selectable(item_button, display_idx));
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
                .padding(row_padding(fs))
                .width(Length::Fill)
                .style(item_button_style(t, is_selected, t.text_muted));

            items_column = items_column.push(self.selectable(loading_button, special_item_idx));
            special_item_idx += 1;
        } else if let Some(help_msg) = &self.script_filter.help_message {
            let help_row = Row::new()
                .spacing(16)
                .align_y(iced::Alignment::Center)
                .push(text(help_msg.clone()).size(fs.item).color(t.text_muted));

            let is_selected = self.selected_index == special_item_idx;
            let help_button = button(help_row)
                .padding(row_padding(fs))
                .width(Length::Fill)
                .style(item_button_style(t, is_selected, t.text_muted));

            items_column = items_column.push(self.selectable(help_button, special_item_idx));
            special_item_idx += 1;
        } else if let Some(sf_result) = &self.script_filter.results {
            let guides = tree_guide_segments(&sf_result.items);
            for (item, guides) in sf_result.items.iter().zip(guides) {
                let is_selected = self.selected_index == special_item_idx;
                let mut item_row = Row::new().spacing(16).align_y(iced::Alignment::Center);

                let icon_path = self.script_filter_icon(item, sf_result);

                if let Some(icon_path_str) = icon_path {
                    if let Some(icon_content) = icon_element(Path::new(&icon_path_str), 40.0) {
                        item_row = item_row.push(icon_content);
                    }
                }

                let mut text_col = Column::new();
                text_col = text_col.push(rich_text(ansi_to_spans(
                    &item.title,
                    fs.item,
                    t.text_main,
                    iced::Font::default(),
                    &self.view.preview_style.ansi,
                )));
                if let Some(subtitle) = &item.subtitle {
                    text_col = text_col.push(rich_text(ansi_to_spans(
                        subtitle,
                        fs.subtitle,
                        t.text_muted,
                        iced::Font::default(),
                        &self.view.preview_style.ansi,
                    )));
                }
                item_row = item_row.push(text_col.width(Length::Fill));

                let item_button = button(item_row)
                    .on_press(Message::ItemClicked(special_item_idx))
                    .padding(iced::Padding {
                        left: fs.item_padding + indent_left(item.indent),
                        ..row_padding(fs)
                    })
                    .width(Length::Fill)
                    .style(item_button_style(t, is_selected, t.text_main));

                items_column = items_column.push(self.selectable(
                    with_tree_guide(item_button, guides, fs.item_padding, t),
                    special_item_idx,
                ));
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
                .padding(row_padding(fs))
                .width(Length::Fill)
                .style(item_button_style(t, is_selected, t.text_muted));

            items_column = items_column.push(self.selectable(loading_button, special_item_idx));
            special_item_idx += 1;
        } else if self.text_snippets.active {
            if let Some(error) = &self.text_snippets.error {
                let error_row = Column::new().spacing(16).push(
                    text(format!("Snippet failed: {error}"))
                        .size(fs.item)
                        .color(t.ansi[1]),
                );

                let error_button = button(
                    error_row.push(text("Retry · Ctrl+R").size(fs.subtitle).color(t.accent)),
                )
                .on_press(Message::RetryLoad)
                .padding(row_padding(fs))
                .width(Length::Fill)
                .style(move |_theme, _status| button::Style {
                    text_color: t.ansi[1],
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
                        .padding(row_padding(fs))
                        .width(Length::Fill)
                        .style(item_button_style(t, is_selected, t.text_main));

                    items_column =
                        items_column.push(self.selectable(item_button, special_item_idx));
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
                        .padding(row_padding(fs))
                        .width(Length::Fill)
                        .style(item_button_style(t, is_selected, t.text_main));

                    items_column =
                        items_column.push(self.selectable(item_button, special_item_idx));
                    special_item_idx += 1;
                }
            }
        }

        if self.clipboard.active {
            let state = &self.clipboard;
            if state.items.is_empty() {
                let is_selected = self.selected_index == special_item_idx;
                let mut status = Column::new()
                    .spacing(8)
                    .push(text(state.status()).size(fs.item).color(t.text_muted));
                let recovery = self.clipboard_status_recovery();
                if let Some((label, _)) = &recovery {
                    status = status.push(
                        text(format!("Enter: {label}"))
                            .size(fs.subtitle)
                            .color(t.accent),
                    );
                }
                let mut control = button(status)
                    .padding(row_padding(fs))
                    .width(Length::Fill)
                    .style(item_button_style(t, is_selected, t.text_muted));
                if let Some((_, message)) = recovery {
                    control = control.on_press(message);
                }
                items_column = items_column.push(self.selectable(control, special_item_idx));
                special_item_idx += 1;
            }

            let now = super::clipboard_store::now_secs();
            for item in &state.items {
                let is_selected = self.selected_index == special_item_idx;
                let (title, subtitle): (std::borrow::Cow<'_, str>, std::borrow::Cow<'_, str>) =
                    match state.labels.get(&item.id) {
                        Some((title, subtitle)) => (title.into(), subtitle.into()),
                        None => (item.title().into(), item.subtitle().into()),
                    };
                let text_col = Column::new()
                    .push(text(title).size(fs.item).color(t.text_main))
                    .push(text(subtitle).size(fs.subtitle).color(t.text_muted));
                let mut item_row = Row::new().spacing(16).align_y(iced::Alignment::Center);
                let icon = match item.source.as_ref().and_then(|app| state.icons.get(app)) {
                    Some(known) => known
                        .as_deref()
                        .map(|path| existing_icon_element(Path::new(path), 40.0, 1.0)),
                    None => self
                        .clipboard_source_icon(item)
                        .and_then(|path| icon_element(Path::new(&path), 40.0)),
                };
                if let Some(icon) = icon {
                    item_row = item_row.push(icon);
                }
                let mut item_row = item_row.push(text_col.width(Length::Fill));
                if let Some((label, urgent)) = item.expiry_label(now) {
                    let color = if urgent {
                        iced::Color::from_rgb8(230, 150, 55)
                    } else {
                        t.text_muted
                    };
                    item_row = item_row.push(text(label).size(fs.subtitle).color(color));
                }
                items_column = items_column.push(
                    self.selectable(
                        button(item_row)
                            .on_press(Message::ItemClicked(special_item_idx))
                            .padding(row_padding(fs))
                            .width(Length::Fill)
                            .style(item_button_style(t, is_selected, t.text_main)),
                        special_item_idx,
                    ),
                );
                special_item_idx += 1;
            }
        }

        if let Some(status) = self.command_list.status() {
            let is_selected = self.selected_index == special_item_idx;
            let mut status = Column::new()
                .spacing(8)
                .push(text(status.to_string()).size(fs.item).color(t.text_muted));
            let mut item = None;
            if let Some((label, message)) = self.command_status_recovery() {
                status = status.push(
                    text(format!("Enter: {label}"))
                        .size(fs.subtitle)
                        .color(t.accent),
                );
                item = Some(message);
            }
            let mut control = button(status)
                .padding(row_padding(fs))
                .width(Length::Fill)
                .style(item_button_style(t, is_selected, t.text_muted));
            if let Some(message) = item {
                control = control.on_press(message);
            }
            items_column = items_column.push(self.selectable(control, special_item_idx));
            special_item_idx += 1;
        } else if self.command_list.is_active() {
            // Section headings only while nothing is typed: a filter lists
            // the best matches first, whatever their section.
            let grouping = self.command_list.query.is_empty();
            let group_of = |row: usize| {
                self.command_list
                    .item(row)
                    .and_then(|item| item.group.as_ref())
                    .filter(|_| grouping)
            };
            for row in 0..self.command_list.row_count() {
                let Some(item) = self.command_list.item(row) else {
                    continue;
                };
                let group = group_of(row);
                if let Some(group) = group {
                    if row == 0 || group_of(row - 1) != Some(group) {
                        items_column = items_column.push(self.row_group_header(group));
                    }
                }
                let is_selected = self.selected_index == special_item_idx;
                // Faded towards the background so dimmed rows stand apart
                // from the others in any theme.
                let faded = |color: iced::Color, alpha: f32| iced::Color {
                    a: color.a * alpha,
                    ..color
                };
                let mut item_row = Row::new().spacing(16).align_y(iced::Alignment::Center);
                let far = self.command_list.addon
                    == Some(super::command_list::CommandListAddon::Slack)
                    && !self.slack_row_near(row, super::slack_mode::ICON_WINDOW);
                if far && item.icon.is_some() {
                    item_row = item_row.push(
                        iced::widget::Space::new()
                            .width(Length::Fixed(40.0))
                            .height(Length::Fixed(40.0)),
                    );
                } else if let Some(icon) = item.icon.as_deref().and_then(|path| {
                    let opacity = if item.dimmed { DIMMED_OPACITY } else { 1.0 };
                    faded_icon_element(Path::new(path), 40.0, opacity)
                }) {
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
                } else if item.dimmed {
                    text(item.subtitle.clone())
                        .size(fs.subtitle)
                        .color(faded(t.text_muted, DIMMED_OPACITY))
                } else {
                    text(item.subtitle.clone())
                        .size(fs.subtitle)
                        .color(t.text_muted)
                };
                let title_color = if item.dimmed {
                    faded(t.text_muted, DIMMED_OPACITY)
                } else {
                    t.text_main
                };
                let text_col = Column::new()
                    .push(
                        text(pinned_title(&item.title, item.pinned))
                            .size(fs.item)
                            .color(title_color),
                    )
                    .push(subtitle);
                item_row = item_row.push(text_col.width(Length::Fill));
                let notes = self
                    .command_list
                    .is_extra_row(row)
                    .then(|| super::bitwarden::NOTES_BADGE.to_string());
                let badge_color = if item.dimmed {
                    faded(t.text_muted, DIMMED_OPACITY)
                } else {
                    t.accent
                };
                for badge in notes.iter().chain(&item.badge) {
                    item_row =
                        item_row.push(tinted_badge_pill(badge.clone(), badge_color, fs.subtitle));
                }
                let indent = if group.is_some() { indent_left(1) } else { 0.0 };
                let item_button = button(item_row)
                    .on_press(Message::ItemClicked(special_item_idx))
                    .padding(iced::Padding {
                        left: fs.item_padding + indent,
                        ..row_padding(fs)
                    })
                    .width(Length::Fill)
                    .style(item_button_style(t, is_selected, t.text_main));
                let row_element = if group.is_some() {
                    let guide = TreeGuideSegments {
                        above: true,
                        below: group_of(row + 1) == group,
                        elbow: true,
                    };
                    with_tree_guide(item_button, vec![guide], fs.item_padding, t)
                } else {
                    item_button.into()
                };
                items_column = items_column.push(self.selectable(row_element, special_item_idx));
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
                    .padding(row_padding(fs))
                    .width(Length::Fill)
                    .style(item_button_style(t, is_selected, t.text_main));
                if row.copyable {
                    item_button = item_button.on_press(Message::ItemClicked(special_item_idx));
                }
                items_column = items_column.push(self.selectable(item_button, special_item_idx));
                special_item_idx += 1;
            }
        }

        if self.file_browser.active {
            if let Some(error) = &self.file_browser.error {
                let error = Column::new()
                    .spacing(8)
                    .push(text(error.clone()).size(fs.item).color(t.ansi[1]))
                    .push(text("Retry · Ctrl+R").size(fs.subtitle).color(t.accent));
                items_column = items_column.push(
                    button(error)
                        .on_press(Message::RetryLoad)
                        .padding(row_padding(fs))
                        .width(Length::Fill)
                        .style(item_button_style(t, false, t.text_muted)),
                );
            }

            for entry in self.file_browser.entries.iter() {
                let is_selected = self.selected_index == special_item_idx;
                let mut item_row = Row::new().spacing(16).align_y(iced::Alignment::Center);

                if !self.icon_map.is_empty() {
                    let icon_name = if entry.is_dir {
                        "folder"
                    } else {
                        mimetype_icon_name(&entry.full_path)
                    };
                    if let Some(icon_path_str) = self.icon_map.get(icon_name) {
                        if let Some(icon_content) = icon_element(Path::new(icon_path_str), 40.0) {
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
                    .padding(row_padding(fs))
                    .width(Length::Fill)
                    .style(item_row_style(t, is_selected, name_color));

                let item_area = mouse_area(item_container)
                    .on_press(Message::FileBrowserRowPressed {
                        index: special_item_idx,
                        path: entry.full_path.clone(),
                    })
                    .on_release(Message::FileBrowserRowReleased(special_item_idx))
                    .interaction(iced::mouse::Interaction::Pointer);

                items_column = items_column.push(self.selectable(item_area, special_item_idx));
                special_item_idx += 1;
            }
        }

        if let Some(ws) = &self.web_search.active {
            let is_selected = self.selected_index == special_item_idx;
            let mut ws_row = Row::new().spacing(16).align_y(iced::Alignment::Center);

            if let Some(icon_name) = &ws.icon {
                if let Some(icon_path_str) = self.icon_map.get(icon_name) {
                    if let Some(icon_content) = icon_element(Path::new(icon_path_str), 40.0) {
                        ws_row = ws_row.push(icon_content);
                    }
                }
            }

            if ws.query.is_empty() {
                let hint = format!("Search {}: type your query...", ws.name);
                ws_row = ws_row.push(text(hint).size(fs.item).color(t.text_muted));

                let ws_button = button(ws_row)
                    .padding(row_padding(fs))
                    .width(Length::Fill)
                    .style(item_button_style(t, is_selected, t.text_muted));

                items_column = items_column.push(self.selectable(ws_button, special_item_idx));
            } else {
                let label = format!("Search {} for '{}'", ws.name, ws.query);
                ws_row = ws_row.push(text(label).size(fs.item).color(t.accent));

                let ws_button = button(ws_row)
                    .on_press(Message::WebSearchSelected)
                    .padding(row_padding(fs))
                    .width(Length::Fill)
                    .style(item_button_style(t, is_selected, t.accent));

                items_column = items_column.push(self.selectable(ws_button, special_item_idx));
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
                .padding(row_padding(fs))
                .width(Length::Fill)
                .style(item_button_style(t, is_selected, t.text_muted));

            items_column = items_column.push(self.selectable(help_button, special_item_idx));
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
                .padding(row_padding(fs))
                .width(Length::Fill)
                .style(item_button_style(t, is_selected, t.text_muted));

            items_column = items_column.push(self.selectable(loading_button, special_item_idx));
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
                .padding(row_padding(fs))
                .width(Length::Fill)
                .style(item_button_style(t, is_selected, t.accent));

            items_column = items_column.push(self.selectable(currency_button, special_item_idx));
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
                .padding(row_padding(fs))
                .width(Length::Fill)
                .style(item_button_style(t, is_selected, t.text_muted));

            items_column = items_column.push(self.selectable(loading_button, special_item_idx));
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
                    .padding(row_padding(fs))
                    .width(Length::Fill)
                    .style(item_button_style(t, is_selected, t.accent));

                items_column =
                    items_column.push(self.selectable(conversion_button, special_item_idx));
                special_item_idx += 1;
            }
        }

        if let Some(error) = &self.currency.error {
            if !self.currency.loading && !self.currency.multi_loading {
                let error_row = Row::new()
                    .spacing(16)
                    .align_y(iced::Alignment::Center)
                    .push(text(error.as_str()).size(fs.item).color(t.text_muted));
                items_column = items_column.push(
                    button(error_row)
                        .padding(row_padding(fs))
                        .width(Length::Fill)
                        .style(item_button_style(t, false, t.text_muted)),
                );
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
                    .padding(row_padding(fs))
                    .width(Length::Fill)
                    .style(item_button_style(t, is_selected, t.accent));

                items_column = items_column.push(self.selectable(inline_button, special_item_idx));
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
                    .padding(row_padding(fs))
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

        if self.total_items() == 0
            && self.file_browser.error.is_none()
            && self.text_snippets.error.is_none()
            && self.script_filter.help_message.is_none()
        {
            let mut empty = Column::new()
                .spacing(12)
                .align_x(iced::Alignment::Center)
                .push(
                    text(self.empty_state_message())
                        .size(fs.subtitle)
                        .color(t.text_muted),
                );
            if let Some((label, message)) = self.empty_state_recovery() {
                empty = empty.push(
                    button(text(label).size(fs.subtitle).color(t.accent))
                        .on_press(message)
                        .padding(8),
                );
            }
            items_column = items_column.push(
                container(empty)
                    .width(Length::Fill)
                    .padding(16)
                    .align_x(iced::Alignment::Center),
            );
        } else {
            for (idx, reference) in self.main_view.ordered_rows.iter().enumerate() {
                let display_idx = idx + special_item_idx;
                let config_idx = match *reference {
                    MainRowRef::More(source) => {
                        let label = format!(
                            "More from {}… ({} more)",
                            self.main_source_label(source),
                            self.main_view.more_count(source)
                        );
                        let selected = self.selected_index == display_idx;
                        let item = button(text(label).size(fs.subtitle).color(t.accent))
                            .on_press(Message::ItemClicked(display_idx))
                            .padding(row_padding(fs))
                            .width(Length::Fill)
                            .style(item_button_style(t, selected, t.text_main));
                        items_column = items_column.push(self.selectable(item, display_idx));
                        continue;
                    }
                    MainRowRef::Source { source, row } => {
                        if let Some(item) =
                            self.main_view_row(source, row, display_idx, &main_view_guides[source])
                        {
                            items_column = items_column.push(item);
                        }
                        continue;
                    }
                    MainRowRef::Launcher(config_idx) => config_idx,
                };
                let config = &self.configs[config_idx];
                let description = config
                    .description
                    .clone()
                    .unwrap_or_else(|| config.binary.clone().unwrap_or_default());

                let mut item_row = Row::new().spacing(16).align_y(iced::Alignment::Center);
                if let Some(icon_path_str) = self.launcher_icon_paths[config_idx].clone() {
                    if let Some(icon_content) = icon_element(Path::new(&icon_path_str), 40.0) {
                        item_row = item_row.push(icon_content);
                    }
                }

                item_row = item_row.push(text(description).size(fs.item).width(Length::Fill));

                let is_selected = display_idx == self.selected_index;
                let item_button = button(item_row)
                    .on_press(Message::ItemClicked(display_idx))
                    .padding(row_padding(fs))
                    .width(Length::Fill)
                    .style(item_button_style(t, is_selected, t.text_main));

                items_column = items_column.push(self.selectable(item_button, display_idx));
            }
        }

        if has_fallbacks {
            let fallback_start_idx = special_item_idx + self.main_view.ordered_rows.len();
            for (fb_idx, fallback) in self.fallbacks.iter().enumerate() {
                let display_idx = fallback_start_idx + fb_idx;
                let is_selected = display_idx == self.selected_index;
                let mut item_row = Row::new().spacing(16).align_y(iced::Alignment::Center);

                if let Some(icon_name) = &fallback.icon {
                    if let Some(icon_path_str) = self.icon_map.get(icon_name) {
                        if let Some(icon_content) = icon_element(Path::new(icon_path_str), 40.0) {
                            item_row = item_row.push(icon_content);
                        }
                    }
                }

                let label = fallback.label(self.search_query.trim());
                let fb_text_color = if is_selected {
                    t.text_main
                } else {
                    t.text_muted
                };
                item_row = item_row.push(text(label).size(fs.item).color(fb_text_color));

                let item_button = button(item_row)
                    .on_press(Message::ItemClicked(display_idx))
                    .padding(row_padding(fs))
                    .width(Length::Fill)
                    .style(item_button_style(t, is_selected, fb_text_color));

                items_column = items_column.push(self.selectable(item_button, display_idx));
            }
        }

        let sep = span(HINT_SEP).size(fs.hint).color(t.border);
        let mut hint_spans: Vec<HintSpan<'_>> = vec![
            span("Ctrl+/").size(fs.hint).color(t.accent),
            span(" keywords").size(fs.hint).color(t.text_muted),
        ];

        if !self.selected_actions().is_empty() {
            hint_spans.insert(0, sep.clone());
            hint_spans.insert(0, span(" Actions").size(fs.hint).color(t.text_muted));
            hint_spans.insert(
                0,
                span(ACTIONS_KEY)
                    .size(fs.hint)
                    .font(iced::Font {
                        weight: iced::font::Weight::Bold,
                        ..iced::Font::DEFAULT
                    })
                    .color(t.accent),
            );
        }
        if let Some(action) = self.empty_search_enter_hint() {
            hint_spans.insert(0, sep.clone());
            hint_spans.insert(
                0,
                span(format!("Enter: {action}"))
                    .size(fs.hint)
                    .color(t.text_main),
            );
        }

        if self.keyword_chips && self.keyword_suggestions.len() > 1 && self.selected_index == 0 {
            hint_spans.push(sep.clone());
            hint_spans.push(span("Left/Right").size(fs.hint).color(t.accent));
            hint_spans.push(span(" plugins").size(fs.hint).color(t.text_muted));
        }
        if let Some(list) = self.visible_command_list() {
            if let Some(error) = &list.refresh_error {
                hint_spans.push(sep.clone());
                hint_spans.push(
                    span(format!("Refresh failed: {error} · Ctrl+R: Retry"))
                        .size(fs.hint)
                        .color(t.ansi[1]),
                );
            } else if list.refreshing {
                hint_spans.push(sep.clone());
                hint_spans.push(
                    span("Refreshing… showing previous results")
                        .size(fs.hint)
                        .color(t.text_muted),
                );
            }
        }
        if let Some(label) = self.cycle_view_label() {
            hint_spans.push(sep.clone());
            hint_spans.push(span("Ctrl+Tab").size(fs.hint).color(t.accent));
            hint_spans.push(span(format!(" {label}")).size(fs.hint).color(t.text_main));
        }
        if self.history.max_items > 0 {
            if !hint_spans.is_empty() {
                hint_spans.push(sep.clone());
            }
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
            hint_spans.push(span("Ctrl+B").size(fs.hint).color(t.accent));
            hint_spans.push(span(" file manager").size(fs.hint).color(t.text_muted));
            hint_spans.push(sep.clone());
            hint_spans.push(span("Ctrl+Y").size(fs.hint).color(t.accent));
            hint_spans.push(span(" copy path").size(fs.hint).color(t.text_muted));
            hint_spans.push(sep.clone());
            hint_spans.push(span("Ctrl+Alt+Y").size(fs.hint).color(t.accent));
            hint_spans.push(span(" copy image").size(fs.hint).color(t.text_muted));
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
        }
        if self.clipboard.active {
            if !hint_spans.is_empty() {
                hint_spans.push(sep.clone());
            }
            // A row inside the list would be scrolled out of view, so errors
            // about a listed item are shown here instead.
            if let Some(error) = self
                .clipboard
                .error
                .as_ref()
                .filter(|_| !self.clipboard.items.is_empty())
            {
                hint_spans.push(span(error.as_str()).size(fs.hint).color(t.ansi[1]));
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
            if !self.offers_f5() {
                hint_spans.push(span("Ctrl+R").size(fs.hint).color(t.accent));
                hint_spans.push(span(" refresh").size(fs.hint).color(t.text_muted));
                hint_spans.push(sep.clone());
            }
            hint_spans.push(span("Ctrl+D").size(fs.hint).color(t.accent));
            hint_spans.push(span(" delete").size(fs.hint).color(t.text_muted));
        }
        if self.snippet_save_open() {
            hint_spans.clear();
            if let Some(error) = &self.clipboard.error {
                hint_spans.push(span(error.as_str()).size(fs.hint).color(t.ansi[1]));
                hint_spans.push(sep.clone());
            }
            hint_spans.push(span("Enter").size(fs.hint).color(t.accent));
            let action = match self.snippet_save.as_ref().map(|save| save.name.is_some()) {
                Some(true) => " save",
                _ => " next",
            };
            hint_spans.push(span(action).size(fs.hint).color(t.text_muted));
            hint_spans.push(sep.clone());
            hint_spans.push(span("Esc").size(fs.hint).color(t.accent));
            hint_spans.push(span(" cancel").size(fs.hint).color(t.text_muted));
            return self.shell("", hint_spans, items_column);
        }
        if self.vault.prompt.is_some() {
            hint_spans.clear();
            hint_spans.push(span("Enter").size(fs.hint).color(t.accent));
            hint_spans.push(span(" unlock").size(fs.hint).color(t.text_muted));
            hint_spans.push(sep.clone());
            hint_spans.push(span("Esc").size(fs.hint).color(t.accent));
            hint_spans.push(span(" cancel").size(fs.hint).color(t.text_muted));
            return self.shell("", hint_spans, items_column);
        }
        if self.vault_addon().is_some() {
            if !hint_spans.is_empty() {
                hint_spans.push(sep.clone());
            }
            for (index, (key, label)) in [
                ("Enter", " copy"),
                ("Ctrl+Enter", " type"),
                ("Ctrl+U", " user"),
                ("Ctrl+O", " OTP"),
            ]
            .into_iter()
            .enumerate()
            {
                if index > 0 {
                    hint_spans.push(sep.clone());
                }
                hint_spans.push(span(key).size(fs.hint).color(t.accent));
                hint_spans.push(span(label).size(fs.hint).color(t.text_muted));
            }
        }
        if self.command_list.addon == Some(super::command_list::CommandListAddon::Slack)
            && !self.offers_f5()
        {
            if !hint_spans.is_empty() {
                hint_spans.push(sep.clone());
            }
            hint_spans.push(span("Ctrl+R").size(fs.hint).color(t.accent));
            hint_spans.push(span(" refresh").size(fs.hint).color(t.text_muted));
        }
        for hint in self.server_hints() {
            if !hint_spans.is_empty() {
                hint_spans.push(sep.clone());
            }
            if let Some(key) = hint.key {
                hint_spans.push(span(key).size(fs.hint).color(t.accent));
            }
            let color = match hint.tone {
                super::server_refresh::Tone::Muted => t.text_muted,
                super::server_refresh::Tone::Warning => t.ansi[3],
                super::server_refresh::Tone::Error => t.ansi[1],
            };
            hint_spans.push(span(hint.label).size(fs.hint).color(color));
        }
        if !hint_spans.is_empty() {
            hint_spans.push(sep.clone());
        }
        hint_spans.push(span("Ctrl+P").size(fs.hint).color(t.accent));
        hint_spans.push(span(" preview").size(fs.hint).color(t.text_muted));

        hint_spans = fit_hints(
            hint_spans,
            (self.view.window_size.width - 40.0).max(200.0),
            fs.hint,
            self.clipboard.active && !self.no_icons,
            Some(KeyCap::new(t.accent)),
        );

        if self.file_browser.dragging {
            hint_spans.clear();
            hint_spans.push(
                span("Release over a target to drop the file")
                    .size(fs.hint)
                    .color(t.accent),
            );
        }

        self.shell(&self.view.placeholder, hint_spans, items_column)
    }

    pub(super) fn subscription(&self) -> iced::Subscription<Message> {
        use iced::keyboard;
        use iced::keyboard::key::Named;
        use iced::mouse;
        use iced::window;
        use iced::{event, Event};

        let keys = event::listen_with(|event, _status, _id| match event {
            Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(Named::ArrowDown),
                ..
            }) => Some(Message::MoveDown),
            Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(Named::ArrowUp),
                ..
            }) => Some(Message::MoveUp),
            Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(Named::ArrowLeft),
                modifiers,
                ..
            }) if modifiers.is_empty() => Some(Message::ChipMove(true)),
            Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(Named::ArrowRight),
                modifiers,
                ..
            }) if modifiers.is_empty() => Some(Message::ChipMove(false)),
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
                modifiers,
                ..
            }) if modifiers.control() => Some(Message::CycleView(modifiers.shift())),
            Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(Named::Tab),
                modifiers,
                ..
            }) => Some(if modifiers.shift() {
                Message::BackTabComplete
            } else {
                Message::TabComplete
            }),
            Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(Named::F5),
                modifiers,
                repeat: false,
                ..
            }) => server_refresh_message(modifiers),
            Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Character(ref c),
                modifiers,
                ..
            }) if c.as_str() == "v" && modifiers.control() && modifiers.alt() => {
                Some(Message::ToggleVimMode)
            }
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
            }) if modifiers.control() && matches!(c.as_str(), "o" | "b" | "y") => {
                file_browser_shortcut_message(c.as_str(), modifiers)
            }
            Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Character(ref c),
                modifiers,
                ..
            }) if c.as_str() == "s" && modifiers.control() => Some(Message::FileBrowserCycleSort),
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
            }) if c.as_str() == "d" && modifiers.control() => Some(Message::DeleteSelected),
            Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Character(ref c),
                modifiers,
                ..
            }) if c.as_str() == "u" && modifiers.control() => Some(Message::ClearQuery),
            Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Character(ref c),
                modifiers,
                ..
            }) if c.as_str() == "w" && modifiers.control() => Some(Message::DeleteWord),
            Event::Keyboard(ref key_event)
                if matches!(key_event, keyboard::Event::KeyPressed { .. }) =>
            {
                vim_key_message(key_event)
            }
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
        });
        let compositor = (self.command_list.addon
            == Some(super::command_list::CommandListAddon::Windows))
        .then(self.compositor)
        .flatten();
        let expiry = if self.clipboard.active
            && (self.addons.clipboard.ocr.enabled
                || self
                    .clipboard
                    .items
                    .iter()
                    .any(|item| item.expires_at.is_some()))
        {
            iced::Subscription::run_with(true, |_| super::clipboard_mode::expiry_tick())
                .map(|()| Message::ClipboardTick)
        } else {
            iced::Subscription::none()
        };
        match compositor {
            Some(compositor) => iced::Subscription::batch([
                keys,
                iced::Subscription::run_with(compositor, super::window_events::watch)
                    .map(|()| Message::WindowsChanged),
                expiry,
            ]),
            None => iced::Subscription::batch([keys, expiry]),
        }
    }
}
