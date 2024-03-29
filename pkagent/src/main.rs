use futures::{channel::mpsc, prelude::*};
use glib::clone;
use iced_native::widget::{self, *};
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    convert::TryInto,
    rc::Rc,
};
use wstk::*;

static OBJ_PATH: &str = "/technology/unrelenting/waysmoke/Agent";

enum DialogElement {
    Info(String),
    Error(String),
    Prompt { txt: String, echo_on: bool, done: bool },
}

struct AuthRequest {
    action_id: String,
    message: String,
    icon_name: String,
    cookie: String,
    identities: Vec<(String, HashMap<String, glib::Variant>)>,
    begin_invo: gio::DBusMethodInvocation,
}

struct AuthRunState {
    req: AuthRequest,
    session: polkit_agent::Session,
    notifier: event_listener::Event,
    elements: RefCell<Vec<DialogElement>>,
    done: Cell<bool>,
}

struct AuthRun<'a> {
    state: Rc<AuthRunState>,
    dialog: MultiMonitor<'a, IcedInstance<AuthDialog>>,
}

pub struct AuthAgent<'a> {
    env: &'static Environment<Env>,
    display: &'static Display,
    obj_reg_id: gio::RegistrationId,
    authority: polkit::Authority,
    pk_registered: bool,
    req_rx: mpsc::UnboundedReceiver<AuthRequest>,
    cancel_rx: mpsc::UnboundedReceiver<String>,
    cur_dialog: Option<AuthRun<'a>>,
}

impl<'a> AuthAgent<'a> {
    pub async fn new(
        bus: &'a gio::DBusConnection,
        env: &'static Environment<Env>,
        display: &'static Display,
    ) -> AuthAgent<'a> {
        // the Authority wrapper does not take a bus connection, but it uses bus_get just like we do
        let authority = polkit::Authority::async__future().await.unwrap();
        let (req_tx, req_rx) = mpsc::unbounded();
        let (cancel_tx, cancel_rx) = mpsc::unbounded();

        let obj_reg_id = reg_object(bus, req_tx, cancel_tx).await;
        let pk_registered = reg_with_polkit(&authority).await;

        // TODO: authority.connect_property_owner_notify()

        AuthAgent {
            env,
            display,
            obj_reg_id,
            authority,
            pk_registered,
            req_rx,
            cancel_rx,
            cur_dialog: None,
        }
    }
}

#[async_trait(?Send)]
impl<'a> Runnable for AuthAgent<'a> {
    async fn run(&mut self) -> bool {
        let this = self;
        if let Some(run) = this.cur_dialog.as_mut() {
            futures::select_biased! {
                cookie = this.cancel_rx.select_next_some().fuse() => {
                    if run.state.req.cookie == cookie {
                        this.cur_dialog = None;
                    }
                    // TODO: support canceling queued reqs
                },
                cont = run.dialog.run().fuse() => {
                    if !cont {
                        this.cur_dialog = None;
                    }
                }
            }
        } else {
            // TODO: support canceling queued reqs (select here too)
            let req = this.req_rx.select_next_some().await;
            let env = this.env;
            let display = this.display;
            if this.cur_dialog.is_none() {
                let session =
                    polkit_agent::Session::new(&convert_ident(req.identities.first().unwrap()).unwrap(), &req.cookie);
                let state = Rc::new(AuthRunState {
                    req,
                    session,
                    notifier: event_listener::Event::new(),

                    elements: RefCell::new(Vec::new()),
                    done: Cell::new(false),
                });
                state
                    .session
                    .connect_request(clone!(@strong state => move |_s, prompt, echo_on| {
                        state.elements.borrow_mut().push(DialogElement::Prompt {
                            txt: prompt.to_string(),
                            echo_on,
                            done: false,
                        });
                        state.notifier.notify(usize::MAX);
                    }));
                state
                    .session
                    .connect_show_error(clone!(@strong state => move |_s, err| {
                        state.elements.borrow_mut().push(DialogElement::Error(err.to_string()));
                        state.notifier.notify(usize::MAX);
                    }));
                state
                    .session
                    .connect_show_info(clone!(@strong state => move |_s, info| {
                        state.elements.borrow_mut().push(DialogElement::Info(info.to_string()));
                        state.notifier.notify(usize::MAX);
                    }));
                state
                    .session
                    .connect_completed(clone!(@strong state => move |_s, _success| {
                        state.done.replace(true);
                        state.notifier.notify(usize::MAX);
                    }));
                state.session.initiate();
                this.cur_dialog = Some(AuthRun {
                    state: state.clone(),
                    dialog: MultiMonitor::new(
                        Box::new(move |output, _output_info| {
                            IcedInstance::new(AuthDialog::new(state.clone()), env.clone(), display.clone(), output)
                                .boxed_local()
                        }),
                        this.env,
                    )
                    .await,
                });
            } else {
                // TODO: enqueue
            }
        }
        true
    }
}

async fn reg_with_polkit(authority: &polkit::Authority) -> bool {
    let pid = std::process::id().try_into().unwrap();
    authority
        .register_authentication_agent_future(
            &polkit::UnixSession::new_for_process_future(pid).await.unwrap().unwrap(),
            "C.UTF-8", // TODO: read env vars
            OBJ_PATH,
        )
        .await
        .is_ok()
}

async fn reg_object(
    bus: &gio::DBusConnection,
    req_tx: mpsc::UnboundedSender<AuthRequest>,
    cancel_tx: mpsc::UnboundedSender<String>,
) -> gio::RegistrationId {
    let intf_agent = gio::DBusNodeInfo::for_xml(include_str!("org.freedesktop.PolicyKit1.AuthenticationAgent.xml"))
        .unwrap()
        .lookup_interface("org.freedesktop.PolicyKit1.AuthenticationAgent")
        .unwrap();
    // silly rust wrapper, gdbus won't move to a new thread
    let req_tx = fragile::Fragile::new(RefCell::new(req_tx));
    let cancel_tx = fragile::Fragile::new(RefCell::new(cancel_tx));
    bus.register_object(
        OBJ_PATH,
        &intf_agent,
        move |_conn, uniq, path, intf, meth, args, invo| {
            // eprintln!("Server method call: {} {} {} {}: {:?}", uniq, path, intf, meth, args);
            match meth {
                "BeginAuthentication" => {
                    if let Some((action_id, message, icon_name, _details, cookie, identities)) = args.get::<(
                        String,
                        String,
                        String,
                        HashMap<String, String>,
                        String,
                        Vec<(String, HashMap<String, glib::Variant>)>,
                    )>(
                    ) {
                        let req = AuthRequest {
                            action_id,
                            message,
                            icon_name,
                            cookie,
                            identities,
                            begin_invo: invo,
                        };
                        if let Err(e) = req_tx.get().borrow_mut().unbounded_send(req) {
                            if !e.is_disconnected() {
                                panic!("Unexpected send error {:?}", e)
                            }
                        }
                    } else {
                        eprintln!("WTF"); // prevented by gdbus
                        invo.return_value(None);
                    }
                }
                "CancelAuthentication" => {
                    if let Some((cookie,)) = args.get::<(String,)>() {
                        if let Err(e) = cancel_tx.get().borrow_mut().unbounded_send(cookie) {
                            if !e.is_disconnected() {
                                panic!("Unexpected send error {:?}", e)
                            }
                        }
                    } else {
                        eprintln!("WTF"); // prevented by gdbus
                    }
                    invo.return_value(None);
                }
                _ => {
                    eprintln!("WTF"); // prevented by gdbus
                    invo.return_value(None);
                }
            }
        },
        |_conn, _uniq, _path, _intf, _prop| {
            use glib::ToVariant;
            1337_i32.to_variant()
        },
        |_conn, _uniq, _path, _intf, _prop, _val| false,
    )
    .unwrap()
}

// have to do this because we don't use libpolkit-agent's class for listening
fn convert_ident(ident: &(String, HashMap<String, glib::Variant>)) -> Option<polkit::Identity> {
    let (ref s, ref attrs) = ident;
    if s == "unix-user" {
        return Some(polkit::UnixUser::new(
            attrs.get("uid")?.get::<u32>()?.try_into().unwrap(),
        ));
    }
    // TODO unix-group, unix-netgroup (how are they even used?)
    None
}

#[derive(Debug, Clone)]
enum Msg {
    InputChange(String),
    SubmitResponse,
    CancelResponse,
}

struct AuthDialog {
    st: Rc<AuthRunState>,
    input: widget::text_input::State,
    input_val: String,
    cancel_btn: widget::button::State,
    submit_btn: widget::button::State,
}

impl AuthDialog {
    pub fn new(st: Rc<AuthRunState>) -> AuthDialog {
        AuthDialog {
            st,
            input: widget::text_input::State::focused(),
            input_val: "".to_string(),
            cancel_btn: Default::default(),
            submit_btn: Default::default(),
        }
    }
}

impl DesktopSurface for AuthDialog {
    fn setup_lsh(&self, layer_surface: &Main<layer_surface::ZwlrLayerSurfaceV1>) {
        layer_surface.set_anchor(
            layer_surface::Anchor::Left
                | layer_surface::Anchor::Top
                | layer_surface::Anchor::Right
                | layer_surface::Anchor::Bottom,
        );
        layer_surface.set_exclusive_zone(-1);
        layer_surface.set_keyboard_interactivity(layer_surface::KeyboardInteractivity::Exclusive);
    }
}

#[async_trait(?Send)]
impl IcedSurface for AuthDialog {
    type Message = Msg;

    fn view(&mut self) -> Element<Self::Message> {
        use iced_native::*;

        let title = Text::new(self.st.req.message.clone())
            .size(20)
            .horizontal_alignment(alignment::Horizontal::Center);

        let mut elems = Column::new().spacing(16).push(title);

        // We're only supposed to have one not-done prompt, but Rust doesn't know.
        // Enforce this by borrowing outside of the loop here.
        let mut input_stref = Some(&mut self.input);

        for e in self.st.elements.borrow().iter() {
            match e {
                DialogElement::Info(txt) => {
                    // TODO: style
                    elems = elems.push(Text::new(txt.clone()).size(18));
                }
                DialogElement::Error(txt) => {
                    // TODO: style
                    elems = elems.push(Text::new(txt.clone()).size(18));
                }
                DialogElement::Prompt { txt, echo_on, done } => {
                    let mut row = Row::new()
                        .align_items(Alignment::Center)
                        .spacing(8)
                        .push(Text::new(txt.clone()).size(18));
                    if !done {
                        if let Some(input) = input_stref.take() {
                            let mut input = TextInput::new(input, "", &self.input_val, Msg::InputChange)
                                .on_submit(Msg::SubmitResponse)
                                .width(Length::Fill)
                                .style(style::Dialog)
                                .padding(4);
                            if !echo_on {
                                input = input.password();
                            }
                            row = row.push(input);
                        }
                    }
                    elems = elems.push(row);
                }
            }
        }

        elems = elems.push(
            Row::new()
                .spacing(16)
                .push(
                    Button::new(
                        &mut self.cancel_btn,
                        Text::new("Cancel")
                            .size(18)
                            .horizontal_alignment(alignment::Horizontal::Center),
                    )
                    .on_press(Msg::CancelResponse)
                    .width(Length::Fill)
                    .style(style::Action(style::ActionType::Bad)),
                )
                .push(
                    Button::new(
                        &mut self.submit_btn,
                        Text::new("OK")
                            .size(18)
                            .horizontal_alignment(alignment::Horizontal::Center),
                    )
                    .on_press(Msg::SubmitResponse)
                    .width(Length::Fill)
                    .style(style::Action(style::ActionType::Good)),
                ),
        );

        let dialog = Container::new(elems)
            .style(style::Dialog)
            .width(Length::Units(420))
            .padding(16);

        Container::new(Column::new().push(dialog))
            .style(style::DarkBar)
            .width(Length::Fill)
            .height(Length::Fill)
            .center_x()
            .center_y()
            .into()
    }

    fn retained_images(&mut self) -> Vec<wstk::ImageHandle> {
        vec![]
    }

    async fn update(&mut self, message: Self::Message) {
        match message {
            Msg::InputChange(new_input) => self.input_val = new_input,
            Msg::SubmitResponse => {
                let sent_val = self.input_val.clone();
                self.input_val.clear();
                for e in self.st.elements.borrow_mut().iter_mut().rev() {
                    if let DialogElement::Prompt { ref mut done, .. } = e {
                        *done = true;
                        break;
                    }
                }
                self.st.session.response(&sent_val);
            }
            Msg::CancelResponse => self.st.session.cancel(),
        }
    }

    async fn run(&mut self) -> Action {
        // check before listening! on cancel usually the agent is done (and notify is called) *before* we start polling here
        if !self.st.done.get() {
            self.st.notifier.listen().await;
        }
        if self.st.done.get() {
            self.st.req.begin_invo.return_value(None);
            Action::Close
        } else {
            Action::Rerender
        }
    }
}

async fn main_(env: &'static Environment<Env>, display: &'static Display) {
    let system_bus = gio::bus_get_future(gio::BusType::System).await.unwrap();

    let mut pk_agent = AuthAgent::new(&system_bus, env, display).await;

    while pk_agent.run().await {}
}

wstk_main!(main_);
