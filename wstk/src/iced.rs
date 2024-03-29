use iced_graphics::window::Compositor;
pub use iced_native::Rectangle;
use iced_native::{keyboard, mouse, user_interface::Cache, Damage, Point, Renderer, Size, UserInterface};
use iced_wgpu::window::Compositor as WgpuCompositor;

use std::{
    cell::RefCell,
    io::{Read, Write},
    pin::Pin,
    sync::Arc,
    time::Duration,
};

pub use async_trait::async_trait;
pub use futures::{channel::mpsc, future, prelude::*};

use crate::{event_loop::*, run::*, surfaces::*};

pub struct Clipboard {
    env: Environment<Env>,
    seat: wl_seat::WlSeat,
    last_enter_serial: u32,
    received: Arc<RefCell<Option<String>>>,
    paste_inject_tx: Arc<mpsc::UnboundedSender<()>>,
}

impl iced_native::Clipboard for Clipboard {
    // This really should be asynchronous >_<
    // As a workaround this code initiates on the first call and actually returns on the second call.
    // To make the second call happen automatically, the callback injects a Ctrl-V keystroke :D
    fn read(&self) -> Option<String> {
        if let Some(result) = self.received.borrow_mut().take() {
            return Some(result);
        }
        self.env
            .with_data_device(&self.seat, |device| {
                device.with_selection(|offer| {
                    let offer = match offer {
                        Some(offer) => offer,
                        None => {
                            return;
                        }
                    };

                    let has_text = offer.with_mime_types(|types| types.iter().any(|t| t == "text/plain;charset=utf-8"));
                    if !has_text {
                        return;
                    }
                    if let Ok(mut reader) = offer.receive("text/plain;charset=utf-8".into()) {
                        use std::os::unix::io::AsRawFd;
                        let received = self.received.clone();
                        let inject_tx = self.paste_inject_tx.clone();
                        glib::source::unix_fd_add_local(reader.as_raw_fd(), glib::IOCondition::IN, move |_fd, _ioc| {
                            let mut txt = String::new();
                            reader.read_to_string(&mut txt).unwrap();
                            received.borrow_mut().replace(txt);
                            inject_tx.unbounded_send(()).unwrap();
                            glib::Continue(false)
                        });
                    }
                });
            })
            .unwrap();
        None
    }

    fn write(&mut self, contents: String) {
        let data_source = self.env.new_data_source(
            vec!["text/plain;charset=utf-8".into(), "UTF8_STRING".into()],
            move |event, _| match event {
                data_device::DataSourceEvent::Send { mut pipe, .. } => {
                    if let Err(x) = write!(pipe, "{}", contents) {
                        eprintln!("Could not send clipboard text: {:?}", x);
                    }
                }
                _ => (),
            },
        );

        self.env
            .with_data_device(&self.seat, |device| {
                device.set_selection(&Some(data_source), self.last_enter_serial);
            })
            .unwrap();
    }
}

#[derive(Clone)]
pub enum Action {
    DoNothing,
    Rerender,
    Close,
}

#[derive(Clone)]
pub enum ImageHandle {
    Vector(iced_native::svg::Handle),
    Raster(iced_native::image::Handle),
}

pub type Element<'a, Message> = iced_native::Element<'a, Message, iced_wgpu::Renderer>;

#[async_trait(?Send)]
pub trait IcedSurface {
    type Message: std::fmt::Debug + Send;

    fn view(&mut self) -> Element<'_, Self::Message>;
    fn input_region(&self, _width: u32, _height: u32) -> Option<Vec<Rectangle<u32>>> {
        None
    }
    fn retained_images(&mut self) -> Vec<ImageHandle>;

    async fn update(&mut self, message: Self::Message);
    async fn run(&mut self) -> Action;

    async fn on_pointer_enter(&mut self) {}
    async fn on_pointer_leave(&mut self) {}
    async fn on_touch_enter(&mut self) {}
    async fn on_touch_leave(&mut self) {}
}

pub struct IcedInstance<T: IcedSurface> {
    parent: DesktopInstance,
    surface: T,

    // wayland state
    ptr_active: bool,
    kb_active: bool,
    scale: i32,
    leave_timeout: Option<future::Fuse<Pin<Box<dyn Future<Output = ()> + Send + 'static>>>>,
    prev_input_region: Option<Vec<Rectangle<u32>>>,
    touch_point: Option<i32>,
    touch_leave: bool,
    themed_ptr: Option<pointer::ThemedPointer>,
    last_ptr_serial: Option<u32>,
    keyboard_handle: Option<Main<wl_keyboard::WlKeyboard>>,
    keyboard_events: mpsc::UnboundedReceiver<seat::keyboard::Event>,
    ptr: Option<AsyncMain<wl_pointer::WlPointer>>,
    touch: Option<AsyncMain<wl_touch::WlTouch>>,

    // iced render state
    configured: bool,
    cache: Cache,
    size: Size,
    cursor_position: Point,
    keyboard_mods: keyboard::Modifiers,
    paste_inject_rx: mpsc::UnboundedReceiver<()>,
    compositor: WgpuCompositor,
    renderer: <WgpuCompositor as Compositor>::Renderer,
    gpu_surface: <WgpuCompositor as Compositor>::Surface,
    prev_prim: iced_graphics::Primitive,
    queue: Vec<iced_native::Event>,
    messages: Vec<T::Message>,
    last_mouse_interaction: mouse::Interaction,
    clipboard: Clipboard,
}

impl<T: DesktopSurface + IcedSurface> IcedInstance<T> {
    pub async fn new(
        surface: T,
        env: Environment<Env>,
        display: Display,
        output: wl_output::WlOutput,
    ) -> IcedInstance<T> {
        let parent = DesktopInstance::new(&surface, env.clone(), display, &output);
        let rwh = parent.raw_handle();

        let mut compositor = WgpuCompositor::request(
            iced_wgpu::Settings {
                ..iced_wgpu::Settings::default()
            },
            Some(&rwh),
        )
        .await
        .unwrap();
        let renderer = iced_wgpu::Renderer::new(compositor.create_backend());
        let gpu_surface = compositor.create_surface(&rwh);
        parent.wl_surface.commit();
        parent.flush();

        let seat = &parent.env.get_all_seats()[0];
        let (keyboard_events, keyboard_handle) = if with_seat_data(seat, |d| d.has_keyboard).unwrap() {
            let (hdl, evs) = wayland_keyboard_chan(&seat);
            (evs, Some(hdl))
        } else {
            (futures::channel::mpsc::unbounded().1, None)
        };
        let (ptr, themed_ptr) = if with_seat_data(seat, |d| d.has_pointer).unwrap() {
            (
                Some(AsyncMain::new(seat.get_pointer(), Some(|p| p.release()))),
                Some(parent.theme_mgr.theme_pointer(seat.get_pointer().detach())),
            )
        } else {
            (None, None)
        };
        let touch = if with_seat_data(seat, |d| d.has_touch).unwrap() {
            Some(AsyncMain::new(seat.get_touch(), Some(|p| p.release())))
        } else {
            None
        };

        let (paste_inject_tx, paste_inject_rx) = futures::channel::mpsc::unbounded();

        IcedInstance {
            parent,
            surface,
            ptr_active: false,
            kb_active: false,
            scale: 1,
            leave_timeout: None,
            prev_input_region: None,
            touch_point: None,
            touch_leave: false,
            themed_ptr,
            last_ptr_serial: None,
            keyboard_handle,
            keyboard_events,
            ptr,
            touch,
            configured: false,
            cache: Cache::new(),
            size: Size::new(0.0, 0.0),
            cursor_position: Point::default(),
            keyboard_mods: Default::default(),
            compositor,
            renderer,
            gpu_surface,
            prev_prim: iced_graphics::Primitive::None,
            queue: Vec::new(),
            messages: Vec::new(),
            last_mouse_interaction: mouse::Interaction::Idle,
            clipboard: Clipboard {
                env,
                seat: seat.detach(),
                last_enter_serial: 0,
                received: Arc::new(RefCell::new(None)),
                paste_inject_tx: Arc::new(paste_inject_tx),
            },
            paste_inject_rx,
        }
    }

    fn update_input_region(&mut self) {
        let reg = self.surface.input_region(self.size.width as _, self.size.height as _);
        if reg != self.prev_input_region {
            if let Some(ref rects) = reg {
                let wlreg = self.parent.create_region();
                for rect in rects.iter() {
                    wlreg.add(
                        rect.x.try_into().unwrap(),
                        rect.y.try_into().unwrap(),
                        rect.width.try_into().unwrap(),
                        rect.height.try_into().unwrap(),
                    );
                }
                self.parent.set_input_region(wlreg);
            } else {
                self.parent.clear_input_region();
            }
            self.parent.wl_surface.commit();
        }
        self.prev_input_region = reg;
    }

    fn apply_mouse_interaction(&mut self, interaction: mouse::Interaction) {
        if let Some(ref tptr) = self.themed_ptr {
            use iced_native::mouse::Interaction::*;
            let _ = tptr.set_cursor(
                match interaction {
                    Idle => "default",
                    Pointer => "pointer",
                    Grab => "dnd-ask",
                    Text => "text",
                    Crosshair => "cross",
                    Working => "wait",
                    Grabbing => "dnd-move",
                    ResizingHorizontally => "col-resize",
                    ResizingVertically => "row-resize",
                },
                self.last_ptr_serial,
            );
            self.last_mouse_interaction = interaction;
        }
    }

    async fn render(&mut self) {
        if !self.configured {
            return;
        }

        for h in self.surface.retained_images() {
            match h {
                ImageHandle::Raster(h) => self.renderer.backend_mut().retain_raster(&h),
                ImageHandle::Vector(h) => self.renderer.backend_mut().retain_vector(&h),
            }
        }

        let mut user_interface =
            UserInterface::build(self.surface.view(), self.size, self.cache.clone(), &mut self.renderer);
        user_interface.update(
            &self.queue.drain(..).collect::<Vec<_>>(),
            self.cursor_position,
            &mut self.renderer,
            &mut self.clipboard,
            &mut self.messages,
        );
        let viewport = iced_graphics::Viewport::with_physical_size(
            iced_graphics::Size::new(
                self.size.width as u32 * self.scale as u32,
                self.size.height as u32 * self.scale as u32,
            ),
            self.scale as _,
        );

        if self.messages.is_empty() {
            let mi = user_interface.draw(&mut self.renderer, self.cursor_position);
            let next_prim = self.renderer.damage_token();
            let dmg = self.prev_prim.damage(&next_prim);
            self.prev_prim = next_prim;
            if dmg == None || dmg.map(|x| x.len()).unwrap_or(0) == 0 {
                self.cache = user_interface.into_cache();
                self.update_input_region();
                return;
            }
            self.compositor
                .present::<String>(
                    &mut self.renderer,
                    &mut self.gpu_surface,
                    &viewport,
                    iced_core::Color::TRANSPARENT,
                    &[],
                )
                .unwrap();
            self.cache = user_interface.into_cache();
            self.apply_mouse_interaction(mi);
        } else {
            // iced-winit says we are forced to rebuild twice
            let temp_cache = user_interface.into_cache();

            for message in self.messages.drain(..) {
                self.surface.update(message).await;
            }
            self.parent.flush();

            let mut user_interface =
                UserInterface::build(self.surface.view(), self.size, temp_cache, &mut self.renderer);
            let mi = user_interface.draw(&mut self.renderer, self.cursor_position);
            let next_prim = self.renderer.damage_token();
            let dmg = self.prev_prim.damage(&next_prim);
            self.prev_prim = next_prim;
            if dmg == None || dmg.map(|x| x.len()).unwrap_or(0) == 0 {
                self.cache = user_interface.into_cache();
                return;
            }
            self.compositor
                .present::<String>(
                    &mut self.renderer,
                    &mut self.gpu_surface,
                    &viewport,
                    iced_core::Color::TRANSPARENT,
                    &[],
                )
                .unwrap();
            self.cache = user_interface.into_cache();
            self.apply_mouse_interaction(mi);
        }
        self.update_input_region();
    }

    fn configure_surface(&mut self) {
        self.compositor.configure_surface(
            &mut self.gpu_surface,
            self.size.width as u32 * self.scale as u32,
            self.size.height as u32 * self.scale as u32,
        );
        self.parent.wl_surface.set_buffer_scale(self.scale);
        self.prev_prim = iced_graphics::Primitive::None; // force damage
        self.configured = true;
    }

    async fn on_scale(&mut self, scale: i32) {
        if scale == self.scale {
            return;
        }
        self.scale = scale;
        self.configure_surface();
        self.render().await;
    }

    async fn on_layer_event(&mut self, event: layer_surface::Event) -> bool {
        match event {
            layer_surface::Event::Configure { serial, width, height } => {
                self.parent.layer_surface.ack_configure(serial);

                let new_scale = get_surface_scale_factor(&self.parent.wl_surface);
                let new_size = Size::new(width as f32, height as f32);
                if new_scale != self.scale || new_size != self.size {
                    self.scale = new_scale;
                    self.size = new_size;
                    self.configure_surface();
                    self.render().await;
                }
                true
            }
            layer_surface::Event::Closed { .. } => false,
            _ => {
                eprintln!("unknown lsh event");
                true
            }
        }
    }

    async fn on_keyboard_event(&mut self, event: seat::keyboard::Event) {
        match event {
            seat::keyboard::Event::Enter { surface, serial, .. } => {
                if self.parent.wl_surface.detach() != surface {
                    return;
                }
                self.kb_active = true;
                self.clipboard.last_enter_serial = serial;
            }
            seat::keyboard::Event::Leave { surface, .. } => {
                if self.parent.wl_surface.detach() != surface {
                    return;
                }
                self.kb_active = true;
            }
            seat::keyboard::Event::Modifiers { modifiers, .. } => {
                if !self.kb_active {
                    return;
                }
                self.keyboard_mods = if modifiers.shift {
                    keyboard::Modifiers::SHIFT
                } else {
                    keyboard::Modifiers::empty()
                } | if modifiers.ctrl {
                    keyboard::Modifiers::CTRL
                } else {
                    keyboard::Modifiers::empty()
                } | if modifiers.alt {
                    keyboard::Modifiers::ALT
                } else {
                    keyboard::Modifiers::empty()
                } | if modifiers.logo {
                    keyboard::Modifiers::LOGO
                } else {
                    keyboard::Modifiers::empty()
                };
                self.queue
                    .push(iced_native::Event::Keyboard(keyboard::Event::ModifiersChanged(
                        self.keyboard_mods,
                    )));
            }
            seat::keyboard::Event::Key {
                keysym, state, utf8, ..
            } => {
                if !self.kb_active {
                    return;
                }
                if let Some(key_code) = convert_key(keysym) {
                    self.queue.push(iced_native::Event::Keyboard(match state {
                        seat::keyboard::KeyState::Pressed => keyboard::Event::KeyPressed {
                            key_code,
                            modifiers: self.keyboard_mods,
                        },
                        seat::keyboard::KeyState::Released => keyboard::Event::KeyReleased {
                            key_code,
                            modifiers: self.keyboard_mods,
                        },
                        _ => panic!("new button state?"),
                    }));
                }
                if state == seat::keyboard::KeyState::Released {
                    self.render().await;
                    return;
                }
                if let Some(ustr) = utf8 {
                    // XXX: iced-winit filters out private use chars here
                    for c in ustr.chars() {
                        self.queue
                            .push(iced_native::Event::Keyboard(keyboard::Event::CharacterReceived(c)));
                    }
                }
                self.render().await;
            }
            _ => (),
        }
    }

    async fn inject_paste(&mut self) {
        if !self.kb_active {
            return;
        }
        let modifiers = keyboard::Modifiers::CTRL;
        self.queue
            .push(iced_native::Event::Keyboard(keyboard::Event::ModifiersChanged(
                modifiers,
            )));
        // release first because the actual user V might still be held
        self.queue
            .push(iced_native::Event::Keyboard(keyboard::Event::KeyReleased {
                key_code: keyboard::KeyCode::V,
                modifiers,
            }));
        self.queue
            .push(iced_native::Event::Keyboard(keyboard::Event::KeyPressed {
                key_code: keyboard::KeyCode::V,
                modifiers,
            }));
        self.queue
            .push(iced_native::Event::Keyboard(keyboard::Event::KeyReleased {
                key_code: keyboard::KeyCode::V,
                modifiers,
            }));
        self.queue
            .push(iced_native::Event::Keyboard(keyboard::Event::ModifiersChanged(
                self.keyboard_mods,
            )));
        self.render().await;
    }

    async fn on_pointer_event(&mut self, event: wl_pointer::Event) {
        match event {
            wl_pointer::Event::Enter { surface, serial, .. } => {
                if self.parent.wl_surface.detach() != surface {
                    return;
                }
                self.ptr_active = true;
                self.leave_timeout = None;
                self.surface.on_pointer_enter().await;
                self.last_ptr_serial = Some(serial);
                self.clipboard.last_enter_serial = serial;
                self.apply_mouse_interaction(self.last_mouse_interaction);
            }
            wl_pointer::Event::Leave { surface, serial, .. } => {
                if self.parent.wl_surface.detach() != surface {
                    return;
                }
                self.ptr_active = false;
                self.leave_timeout = Some(glib::timeout_future(Duration::from_millis(200)).fuse());
                self.last_ptr_serial = Some(serial);
            }
            wl_pointer::Event::Button {
                button, state, serial, ..
            } => {
                if !self.ptr_active {
                    return;
                }
                let btn = match button {
                    0x110 => mouse::Button::Left,
                    0x111 => mouse::Button::Right,
                    0x112 => mouse::Button::Middle,
                    x if x > 0x110 => mouse::Button::Other((x - 0x110) as u8),
                    _ => panic!("low button event code"),
                };
                self.queue.push(iced_native::Event::Mouse(match state {
                    wl_pointer::ButtonState::Pressed => mouse::Event::ButtonPressed(btn),
                    wl_pointer::ButtonState::Released => mouse::Event::ButtonReleased(btn),
                    _ => panic!("new button state?"),
                }));
                self.last_ptr_serial = Some(serial);
            }
            wl_pointer::Event::Motion {
                surface_x, surface_y, ..
            } => {
                if !self.ptr_active {
                    return;
                }
                self.cursor_position = Point::new(surface_x as _, surface_y as _);
                self.queue.push(iced_native::Event::Mouse(mouse::Event::CursorMoved {
                    position: Point {
                        x: surface_x as _,
                        y: surface_y as _,
                    },
                }));
                self.last_ptr_serial = None;
            }
            wl_pointer::Event::Axis { axis, value, .. } => {
                if !self.ptr_active {
                    return;
                }
                self.queue.push(iced_native::Event::Mouse(mouse::Event::WheelScrolled {
                    delta: mouse::ScrollDelta::Pixels {
                        x: if axis == wl_pointer::Axis::HorizontalScroll {
                            -value as _
                        } else {
                            0.0
                        },
                        y: if axis == wl_pointer::Axis::VerticalScroll {
                            -value as _
                        } else {
                            0.0
                        },
                    },
                }));
            }
            wl_pointer::Event::AxisSource { .. } => {}
            wl_pointer::Event::AxisStop { .. } => {}
            wl_pointer::Event::AxisDiscrete { .. } => {}
            wl_pointer::Event::Frame { .. } => {
                self.render().await;
                self.last_ptr_serial = None;
            }
            _ => {
                eprintln!("unhandled pointer event");
            }
        }
    }

    async fn on_touch_event(&mut self, event: wl_touch::Event) {
        match event {
            wl_touch::Event::Down { surface, id, x, y, .. } => {
                if self.parent.wl_surface.detach() != surface {
                    return;
                }
                if self.touch_point.is_some() {
                    return;
                }
                self.touch_point = Some(id);
                self.ptr_active = true;
                self.leave_timeout = None;
                self.cursor_position = Point::new(x as _, y as _);
                self.queue.push(iced_native::Event::Mouse(mouse::Event::CursorMoved {
                    position: Point { x: x as _, y: y as _ },
                }));
                self.surface.on_touch_enter().await;
            }
            wl_touch::Event::Motion { id, x, y, .. } => {
                if self.touch_point != Some(id) {
                    return;
                }
                self.cursor_position = Point::new(x as _, y as _);
                self.queue.push(iced_native::Event::Mouse(mouse::Event::CursorMoved {
                    position: Point { x: x as _, y: y as _ },
                }));
            }
            wl_touch::Event::Up { id, .. } => {
                if self.touch_point != Some(id) {
                    return;
                }
                self.touch_point = None;
                self.queue.push(iced_native::Event::Mouse(mouse::Event::ButtonPressed(
                    mouse::Button::Left,
                )));
                self.queue.push(iced_native::Event::Mouse(mouse::Event::ButtonReleased(
                    mouse::Button::Left,
                )));
                self.touch_leave = true;
            }
            wl_touch::Event::Frame { .. } => {
                self.render().await;
                if self.touch_leave {
                    self.surface.on_touch_leave().await;
                    self.touch_leave = false;
                    self.render().await;
                }
            }
            e => eprintln!("{:?}", e),
        }
    }
}

#[async_trait(?Send)]
impl<T: DesktopSurface + IcedSurface> Runnable for IcedInstance<T> {
    async fn run(&mut self) -> bool {
        // TODO: react to seat caps change
        let this = self; // argh macro weirdness
        let mut term = future::Fuse::terminated();
        let mut leave_timeout = this.leave_timeout.as_mut().unwrap_or_else(|| &mut term);
        futures::select! {
            ev = this.parent.layer_surface.next() => if !this.on_layer_event(ev).await { return false },
            ev = this.keyboard_events.select_next_some() => this.on_keyboard_event(ev).await,
            ev = MaybeFuture::new(this.ptr.as_mut().map(|p| p.next())) => this.on_pointer_event(ev).await,
            ev = MaybeFuture::new(this.touch.as_mut().map(|p| p.next())) => this.on_touch_event(ev).await,
            sc = this.parent.scale_rx.select_next_some() => this.on_scale(sc).await,
            () = this.paste_inject_rx.select_next_some() => this.inject_paste().await,
            ac = this.surface.run().fuse() => match ac {
                Action::DoNothing => (),
                Action::Rerender => {
                    this.parent.flush();
                    this.render().await
                },
                Action::Close => return false,
            },
            () = leave_timeout => {
                this.leave_timeout = None;
                this.surface.on_pointer_leave().await;
                // not getting a pointer frame after the timeout ;)
                this.render().await;
            },
        }
        true
    }
}

impl<T: IcedSurface> Drop for IcedInstance<T> {
    fn drop(&mut self) {
        if let Some(tptr) = self.themed_ptr.take() {
            tptr.release();
        }
        if let Some(kb) = self.keyboard_handle.take() {
            kb.release();
        }
    }
}

fn convert_key(keysym: u32) -> Option<keyboard::KeyCode> {
    use seat::keyboard::keysyms as k;
    match keysym {
        k::XKB_KEY_0 => Some(keyboard::KeyCode::Key0),
        k::XKB_KEY_1 => Some(keyboard::KeyCode::Key1),
        k::XKB_KEY_2 => Some(keyboard::KeyCode::Key2),
        k::XKB_KEY_3 => Some(keyboard::KeyCode::Key3),
        k::XKB_KEY_4 => Some(keyboard::KeyCode::Key4),
        k::XKB_KEY_5 => Some(keyboard::KeyCode::Key5),
        k::XKB_KEY_6 => Some(keyboard::KeyCode::Key6),
        k::XKB_KEY_7 => Some(keyboard::KeyCode::Key7),
        k::XKB_KEY_8 => Some(keyboard::KeyCode::Key8),
        k::XKB_KEY_9 => Some(keyboard::KeyCode::Key9),

        k::XKB_KEY_A | k::XKB_KEY_a => Some(keyboard::KeyCode::A),
        k::XKB_KEY_B | k::XKB_KEY_b => Some(keyboard::KeyCode::B),
        k::XKB_KEY_C | k::XKB_KEY_c => Some(keyboard::KeyCode::C),
        k::XKB_KEY_D | k::XKB_KEY_d => Some(keyboard::KeyCode::D),
        k::XKB_KEY_E | k::XKB_KEY_e => Some(keyboard::KeyCode::E),
        k::XKB_KEY_F | k::XKB_KEY_f => Some(keyboard::KeyCode::F),
        k::XKB_KEY_G | k::XKB_KEY_g => Some(keyboard::KeyCode::G),
        k::XKB_KEY_H | k::XKB_KEY_h => Some(keyboard::KeyCode::H),
        k::XKB_KEY_I | k::XKB_KEY_i => Some(keyboard::KeyCode::I),
        k::XKB_KEY_J | k::XKB_KEY_j => Some(keyboard::KeyCode::J),
        k::XKB_KEY_K | k::XKB_KEY_k => Some(keyboard::KeyCode::K),
        k::XKB_KEY_L | k::XKB_KEY_l => Some(keyboard::KeyCode::L),
        k::XKB_KEY_M | k::XKB_KEY_m => Some(keyboard::KeyCode::M),
        k::XKB_KEY_N | k::XKB_KEY_n => Some(keyboard::KeyCode::N),
        k::XKB_KEY_O | k::XKB_KEY_o => Some(keyboard::KeyCode::O),
        k::XKB_KEY_P | k::XKB_KEY_p => Some(keyboard::KeyCode::P),
        k::XKB_KEY_Q | k::XKB_KEY_q => Some(keyboard::KeyCode::Q),
        k::XKB_KEY_R | k::XKB_KEY_r => Some(keyboard::KeyCode::R),
        k::XKB_KEY_S | k::XKB_KEY_s => Some(keyboard::KeyCode::S),
        k::XKB_KEY_T | k::XKB_KEY_t => Some(keyboard::KeyCode::T),
        k::XKB_KEY_U | k::XKB_KEY_u => Some(keyboard::KeyCode::U),
        k::XKB_KEY_V | k::XKB_KEY_v => Some(keyboard::KeyCode::V),
        k::XKB_KEY_W | k::XKB_KEY_w => Some(keyboard::KeyCode::W),
        k::XKB_KEY_X | k::XKB_KEY_x => Some(keyboard::KeyCode::X),
        k::XKB_KEY_Y | k::XKB_KEY_y => Some(keyboard::KeyCode::Y),
        k::XKB_KEY_Z | k::XKB_KEY_z => Some(keyboard::KeyCode::Z),

        k::XKB_KEY_F1 => Some(keyboard::KeyCode::F1),
        k::XKB_KEY_F2 => Some(keyboard::KeyCode::F2),
        k::XKB_KEY_F3 => Some(keyboard::KeyCode::F3),
        k::XKB_KEY_F4 => Some(keyboard::KeyCode::F4),
        k::XKB_KEY_F5 => Some(keyboard::KeyCode::F5),
        k::XKB_KEY_F6 => Some(keyboard::KeyCode::F6),
        k::XKB_KEY_F7 => Some(keyboard::KeyCode::F7),
        k::XKB_KEY_F8 => Some(keyboard::KeyCode::F8),
        k::XKB_KEY_F9 => Some(keyboard::KeyCode::F9),
        k::XKB_KEY_F10 => Some(keyboard::KeyCode::F10),
        k::XKB_KEY_F11 => Some(keyboard::KeyCode::F11),
        k::XKB_KEY_F12 => Some(keyboard::KeyCode::F12),

        k::XKB_KEY_space => Some(keyboard::KeyCode::Space),
        k::XKB_KEY_slash => Some(keyboard::KeyCode::Slash),
        k::XKB_KEY_backslash => Some(keyboard::KeyCode::Backslash),
        k::XKB_KEY_period => Some(keyboard::KeyCode::Period),
        k::XKB_KEY_comma => Some(keyboard::KeyCode::Comma),
        k::XKB_KEY_colon => Some(keyboard::KeyCode::Colon),
        k::XKB_KEY_semicolon => Some(keyboard::KeyCode::Semicolon),
        k::XKB_KEY_underscore => Some(keyboard::KeyCode::Underline),
        k::XKB_KEY_bracketleft => Some(keyboard::KeyCode::LBracket),
        k::XKB_KEY_bracketright => Some(keyboard::KeyCode::RBracket),
        k::XKB_KEY_apostrophe => Some(keyboard::KeyCode::Apostrophe),
        k::XKB_KEY_at => Some(keyboard::KeyCode::At),
        k::XKB_KEY_grave => Some(keyboard::KeyCode::Grave),
        k::XKB_KEY_caret => Some(keyboard::KeyCode::Caret),
        k::XKB_KEY_plus => Some(keyboard::KeyCode::Plus),
        k::XKB_KEY_minus => Some(keyboard::KeyCode::Minus),
        k::XKB_KEY_asterisk => Some(keyboard::KeyCode::Asterisk),
        k::XKB_KEY_equal => Some(keyboard::KeyCode::Equals),

        k::XKB_KEY_ISO_Left_Tab | k::XKB_KEY_Tab => Some(keyboard::KeyCode::Tab),
        k::XKB_KEY_BackSpace => Some(keyboard::KeyCode::Backspace),
        k::XKB_KEY_Return => Some(keyboard::KeyCode::Enter),
        k::XKB_KEY_Escape => Some(keyboard::KeyCode::Escape),
        k::XKB_KEY_Insert => Some(keyboard::KeyCode::Insert),
        k::XKB_KEY_Home => Some(keyboard::KeyCode::Home),
        k::XKB_KEY_Delete => Some(keyboard::KeyCode::Delete),
        k::XKB_KEY_End => Some(keyboard::KeyCode::End),
        k::XKB_KEY_Page_Down => Some(keyboard::KeyCode::PageDown),
        k::XKB_KEY_Page_Up => Some(keyboard::KeyCode::PageUp),
        k::XKB_KEY_Left => Some(keyboard::KeyCode::Left),
        k::XKB_KEY_Up => Some(keyboard::KeyCode::Up),
        k::XKB_KEY_Right => Some(keyboard::KeyCode::Right),
        k::XKB_KEY_Down => Some(keyboard::KeyCode::Down),
        k::XKB_KEY_XF86Copy => Some(keyboard::KeyCode::Copy),
        k::XKB_KEY_XF86Cut => Some(keyboard::KeyCode::Cut),
        k::XKB_KEY_XF86Paste => Some(keyboard::KeyCode::Paste),

        k::XKB_KEY_Alt_L => Some(keyboard::KeyCode::LAlt),
        k::XKB_KEY_Control_L => Some(keyboard::KeyCode::LControl),
        k::XKB_KEY_Shift_L => Some(keyboard::KeyCode::LShift),
        k::XKB_KEY_Super_L => Some(keyboard::KeyCode::LWin),
        k::XKB_KEY_Alt_R => Some(keyboard::KeyCode::RAlt),
        k::XKB_KEY_Control_R => Some(keyboard::KeyCode::RControl),
        k::XKB_KEY_Shift_R => Some(keyboard::KeyCode::RShift),
        k::XKB_KEY_Super_R => Some(keyboard::KeyCode::RWin),

        _ => None,
    }
}
