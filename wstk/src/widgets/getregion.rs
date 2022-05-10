//! A way to track layout regions of particular areas

use iced_native::*;
use std::cell::Cell;

pub struct GetRegion<'a, Message, Renderer: self::Renderer> {
    // The Cell helps both with the immutable &self in layout(), and with multi-borrows in the consumer
    state: &'a Cell<Rectangle>,
    content: Element<'a, Message, Renderer>,
    horizontal_alignment: Alignment,
    vertical_alignment: Alignment,
}

impl<'a, Message, Renderer> GetRegion<'a, Message, Renderer>
where
    Renderer: self::Renderer,
{
    pub fn new<T>(state: &'a Cell<Rectangle>, content: T) -> Self
    where
        T: Into<Element<'a, Message, Renderer>>,
    {
        GetRegion {
            state,
            content: content.into(),
            horizontal_alignment: Alignment::Start,
            vertical_alignment: Alignment::Start,
        }
    }

    pub fn center_x(mut self) -> Self {
        self.horizontal_alignment = Alignment::Center;
        self
    }

    pub fn center_y(mut self) -> Self {
        self.vertical_alignment = Alignment::Center;
        self
    }
}

impl<'a, Message, Renderer> Widget<Message, Renderer> for GetRegion<'a, Message, Renderer>
where
    Renderer: self::Renderer,
    Message: Clone,
{
    fn width(&self) -> Length {
        Length::Shrink
    }

    fn height(&self) -> Length {
        Length::Shrink
    }

    fn layout(&self, renderer: &Renderer, limits: &layout::Limits) -> layout::Node {
        let limits = limits.width(Length::Shrink).height(Length::Shrink);
        let mut content = self.content.layout(renderer, &limits.loose());
        let size = limits.resolve(content.size());
        content.align(self.horizontal_alignment, self.vertical_alignment, size);
        layout::Node::with_children(size, vec![content])
    }

    fn on_event(
        &mut self,
        event: Event,
        layout: Layout<'_>,
        cursor_position: Point,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
    ) -> event::Status {
        self.content.on_event(
            event,
            layout.children().next().unwrap(),
            cursor_position,
            renderer,
            clipboard,
            shell,
        )
    }

    fn mouse_interaction(
        &self,
        layout: Layout<'_>,
        cursor_position: Point,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        self.content
            .mouse_interaction(layout, cursor_position, viewport, renderer)
    }

    fn draw(
        &self,
        renderer: &mut Renderer,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor_position: Point,
        viewport: &Rectangle,
    ) {
        self.state.set(layout.bounds());
        self.content.draw(
            renderer,
            style,
            layout.children().next().unwrap(),
            cursor_position,
            viewport,
        )
    }
}

impl<'a, Message, Renderer> From<GetRegion<'a, Message, Renderer>> for Element<'a, Message, Renderer>
where
    Renderer: 'a + self::Renderer,
    Message: 'a + Clone,
{
    fn from(x: GetRegion<'a, Message, Renderer>) -> Element<'a, Message, Renderer> {
        Element::new(x)
    }
}
