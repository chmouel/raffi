use iced::advanced::{layout, mouse, overlay, renderer, widget, Clipboard, Layout, Shell, Widget};
use iced::keyboard::{key::Named, Key};
use iced::widget::text_input::{self, cursor, Value};
use iced::{Element, Event, Length};

use super::types::Message;

pub(super) struct ActionInput<'a> {
    pub content: Element<'a, Message>,
    pub value: &'a str,
    pub enabled: bool,
}

pub(super) fn at_end(cursor: cursor::State, length: usize) -> bool {
    matches!(cursor, cursor::State::Index(index) if index == length)
}

impl Widget<Message, iced::Theme, iced::Renderer> for ActionInput<'_> {
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
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &iced::Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &iced::Rectangle,
    ) {
        if self.enabled
            && matches!(
                event,
                Event::Keyboard(iced::keyboard::Event::KeyPressed {
                    key: Key::Named(Named::ArrowRight),
                    modifiers,
                    repeat: false,
                    ..
                }) if modifiers.is_empty()
            )
        {
            let state =
                tree.state.downcast_ref::<text_input::State<
                    <iced::Renderer as iced::advanced::text::Renderer>::Paragraph,
                >>();
            let value = Value::new(self.value);
            if state.is_focused() && at_end(state.cursor().state(&value), value.len()) {
                shell.publish(Message::OpenActions);
                shell.capture_event();
                return;
            }
        }
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
