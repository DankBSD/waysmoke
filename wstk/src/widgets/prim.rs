//! Simple widget for directly rendering a primitive

use iced_native::*;

pub struct Prim {
    primitive: iced_graphics::Primitive,
    width: Length,
    height: Length,
}

impl Prim {
    pub fn new(primitive: iced_graphics::Primitive) -> Self {
        Prim {
            primitive,
            width: Length::Fill,
            height: Length::Fill,
        }
    }

    pub fn width(mut self, width: Length) -> Self {
        self.width = width;
        self
    }

    pub fn height(mut self, height: Length) -> Self {
        self.height = height;
        self
    }
}

impl<Message, Backend> Widget<Message, iced_graphics::Renderer<Backend>> for Prim
where
    Backend: iced_graphics::Backend,
{
    fn width(&self) -> Length {
        self.width
    }

    fn height(&self) -> Length {
        self.height
    }

    fn layout(&self, _renderer: &iced_graphics::Renderer<Backend>, limits: &layout::Limits) -> layout::Node {
        let limits = limits.width(self.width).height(self.height);
        layout::Node::new(limits.resolve(Size::INFINITY))
    }

    fn on_event(
        &mut self,
        _event: Event,
        _layout: Layout<'_>,
        _cursor_position: Point,
        _renderer: &iced_graphics::Renderer<Backend>,
        _clipboard: &mut dyn Clipboard,
        _shell: &mut Shell<'_, Message>,
    ) -> event::Status {
        event::Status::Ignored
    }

    fn mouse_interaction(
        &self,
        _layout: Layout<'_>,
        _cursor_position: Point,
        _viewport: &Rectangle,
        _renderer: &iced_graphics::Renderer<Backend>,
    ) -> mouse::Interaction {
        mouse::Interaction::default()
    }

    fn draw(
        &self,
        renderer: &mut iced_graphics::Renderer<Backend>,
        _style: &renderer::Style,
        layout: Layout<'_>,
        _cursor_position: Point,
        _viewport: &Rectangle,
    ) {
        let b = layout.bounds();
        renderer.with_translation(Vector::new(b.x, b.y), |renderer| {
            renderer.draw_primitive(self.primitive.clone());
        });
    }
}

impl<'a, Message, Backend> Into<Element<'a, Message, iced_graphics::Renderer<Backend>>> for Prim
where
    Backend: iced_graphics::Backend,
{
    fn into(self) -> Element<'a, Message, iced_graphics::Renderer<Backend>> {
        Element::new(self)
    }
}
