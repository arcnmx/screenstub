use std::rc::Rc;
use std::path::PathBuf;
use std::iter;
use input::{InputEvent, EventRef, Key, KeyState, RelativeAxis, AbsoluteAxis};
use tokio_core::reactor::Handle;
use futures::unsync::mpsc as un_mpsc;
use futures::{future, Future, Stream, Sink};
use failure::Error;
use config::ConfigQemuRouting;
use qapi::{qmp, Any, Command};
use qemu::{Qemu, CommandFuture};
use uinput;

pub struct RouteQmp {
    qemu: Rc<Qemu>,
}

impl RouteQmp {
    pub fn new(qemu: Rc<Qemu>) -> Self {
        RouteQmp {
            qemu: qemu,
        }
    }

    fn convert_event(e: &InputEvent) -> Option<qmp::InputEvent> {
        Some(match EventRef::new(e) {
            Ok(EventRef::Key(ref key)) if key.key.is_button() => qmp::InputEvent::btn {
                data: qmp::InputBtnEvent {
                    down: key.key_state() == KeyState::Pressed,
                    button: match key.key {
                        Key::ButtonLeft => qmp::InputButton::left,
                        Key::ButtonMiddle => qmp::InputButton::middle,
                        Key::ButtonRight => qmp::InputButton::right,
                        Key::ButtonWheel => qmp::InputButton::wheel_down,
                        Key::ButtonGearUp => qmp::InputButton::wheel_up,
                        Key::ButtonSide => qmp::InputButton::side,
                        Key::ButtonExtra => qmp::InputButton::extra,
                        _ => return None, // TODO: warn/error/etc
                    },
                },
            },
            Ok(EventRef::Key(ref key)) => qmp::InputEvent::key {
                data: qmp::InputKeyEvent {
                    down: key.key_state() == KeyState::Pressed,
                    key: qmp::KeyValue::number { data: key.key as _ },
                },
            },
            Ok(EventRef::Relative(rel)) => qmp::InputEvent::rel {
                data: qmp::InputMoveEvent {
                    axis: match rel.axis {
                        RelativeAxis::X => qmp::InputAxis::x,
                        RelativeAxis::Y => qmp::InputAxis::y,
                        _ => return None, // TODO: warn/error/etc
                    },
                    value: rel.value as _,
                },
            },
            Ok(EventRef::Absolute(abs)) => qmp::InputEvent::abs {
                data: qmp::InputMoveEvent {
                    axis: match abs.axis {
                        AbsoluteAxis::X => qmp::InputAxis::x,
                        AbsoluteAxis::Y => qmp::InputAxis::y,
                        _ => return None, // TODO: warn/error/etc
                    },
                    value: abs.value as _,
                },
            },
            _ => return None, // TODO: warn/error/etc
        })
    }

    fn convert_events<I: IntoIterator<Item=InputEvent>>(e: I) -> qmp::input_send_event {
        qmp::input_send_event {
            device: Default::default(),
            head: Default::default(),
            events: e.into_iter().map(|ref e| Self::convert_event(e)).filter_map(|e| e).collect(),
        }
    }

    pub fn spawn(&self, handle: &Handle, events: un_mpsc::Receiver<InputEvent>, error_sender: un_mpsc::Sender<Error>) {
        handle.spawn(
            self.qemu.connect_qmp(handle).map_err(Error::from).and_then(|qmp|
                events.map_err(|_| -> Error { unreachable!() }).fold(qmp, |qmp, event|
                    CommandFuture::new(future::ok::<_, Error>(qmp), RouteQmp::convert_events(iter::once(event)))
                    .map(|(_, qmp)| qmp)
                )
            ).map(drop).or_else(|e| error_sender.send(e).map(drop).map_err(drop))
        );
    }
}

pub struct RouteUInput<F: ?Sized, D: ?Sized> {
    qemu: Rc<Qemu>,
    builder: uinput::Builder,
    create: Rc<Box<F>>,
    delete: Rc<Box<D>>,
}

impl<F: ?Sized, D: ?Sized> RouteUInput<F, D> {
    pub fn builder(&mut self) -> &mut uinput::Builder {
        &mut self.builder
    }
}

impl RouteUInput<Fn(PathBuf) -> (String, qmp::object_add), Fn(String) -> qmp::object_del> {
    pub fn new_input_linux(qemu: Rc<Qemu>, id: String, repeat: bool) -> Self {
        Self::new(qemu, uinput::Builder::new(), Rc::new(Box::new(move |path: PathBuf| {
            let path = path.display();
            (id.clone(), qmp::object_add {
                id: id.clone(),
                qom_type: "input-linux".into(),
                props: Some(vec![
                    ("evdev".into(), Any::String(path.to_string())),
                    ("repeat".into(), Any::Bool(repeat)),
                ].into_iter().collect()),
            })
        }) as Box<_>), Rc::new(Box::new(|id| {
            qmp::object_del {
                id: id,
            }
        }) as Box<_>))
    }
}

impl RouteUInput<Fn(PathBuf) -> (String, qmp::device_add), Fn(String) -> qmp::device_del> {
    pub fn new_virtio_host(qemu: Rc<Qemu>, id: String, bus: Option<String>) -> Self {
        Self::new(qemu, uinput::Builder::new(), Rc::new(Box::new(move |path: PathBuf| {
            // TODO: should this be virtio-input-host-pci?
            (id.clone(), qmp::device_add::new("virtio-input-host-device".into(), Some(id.clone()), bus.clone(), vec![
                ("evdev".into(), Any::String(path.display().to_string())),
            ]))
        }) as Box<_>), Rc::new(Box::new(|id| {
            qmp::device_del {
                id: id,
            }
        }) as Box<_>))
    }
}

impl<C: Command + 'static, CD: Command + 'static> RouteUInput<Fn(PathBuf) -> (String, C), Fn(String) -> CD> {
    fn new(qemu: Rc<Qemu>, builder: uinput::Builder, create: Rc<Box<Fn(PathBuf) -> (String, C)>>, delete: Rc<Box<Fn(String) -> CD>>) -> Self {
        RouteUInput {
            qemu: qemu,
            builder: builder,
            create: create,
            delete: delete,
        }
    }

    pub fn spawn(&self, handle: &Handle, events: un_mpsc::Receiver<InputEvent>, error_sender: un_mpsc::Sender<Error>) {
        let create = self.create.clone();
        let delete = self.delete.clone();
        handle.spawn(
            future::result(
                self.builder.create()
                .map(|uinput| (uinput.path().to_owned(), uinput))
                .and_then(|(path, uinput)| uinput.to_sink(handle).map(|uinput| (path, uinput)))
                .map_err(Error::from)
                // TODO: delay to let udev fix permissions on the newly created device!
            ).and_then({
                let qemu = self.qemu.clone();
                let handle = handle.clone();
                move |(path, uinput)| {
                    let (id, c) = create(path);
                    qemu.execute_qmp(&handle, c).map(|_| (id, uinput))
                }
            }).and_then({
                let qemu = self.qemu.clone();
                let handle = handle.clone();
                move |(id, uinput)|
                    events.map_err(|_| -> Error { unreachable!() }).forward(uinput.map_err(Error::from)).map(drop)
                    .then(move |e| qemu.execute_qmp(&handle, delete(id)).map(drop).then(|r| e.and_then(|_| r)))
            }).or_else(|e| error_sender.send(e).map(drop).map_err(drop))
        );
    }
}

pub enum Route {
    InputLinux(RouteUInput<Fn(PathBuf) -> (String, qmp::object_add), Fn(String) -> qmp::object_del>),
    VirtioHost(RouteUInput<Fn(PathBuf) -> (String, qmp::device_add), Fn(String) -> qmp::device_del>),
    Qmp(RouteQmp),
    //Spice(RouteInputSpice),
}

impl Route {
    pub fn new(routing: ConfigQemuRouting, qemu: Rc<Qemu>, id: String, bus: Option<String>, repeat: bool) -> Self {
        match routing {
            ConfigQemuRouting::InputLinux => Route::InputLinux(RouteUInput::new_input_linux(qemu, id, repeat)),
            ConfigQemuRouting::VirtioHost => Route::VirtioHost(RouteUInput::new_virtio_host(qemu, id, bus)),
            ConfigQemuRouting::Qmp => Route::Qmp(RouteQmp::new(qemu)),
        }
    }

    pub fn builder(&mut self) -> Option<&mut uinput::Builder> {
        match *self {
            Route::InputLinux(ref mut uinput) => Some(uinput.builder()),
            Route::VirtioHost(ref mut uinput) => Some(uinput.builder()),
            Route::Qmp(..) => None,
        }
    }

    pub fn spawn(self, handle: &Handle, error_sender: un_mpsc::Sender<Error>) -> un_mpsc::Sender<InputEvent> {
        let (sender, events) = un_mpsc::channel(::EVENT_BUFFER);

        match self {
            Route::InputLinux(ref uinput) => uinput.spawn(handle, events, error_sender),
            Route::VirtioHost(ref uinput) => uinput.spawn(handle, events, error_sender),
            Route::Qmp(ref qmp) => qmp.spawn(handle, events, error_sender),
        }

        sender
    }
}

// TODO: spice input
/*pub struct RouteInputSpice {
}*/
