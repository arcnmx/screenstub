use std::sync::Arc;
use std::path::PathBuf;
use std::task::Poll;
use std::iter;
use input::{InputEvent, EventRef, KeyEvent, Key, KeyState, RelativeAxis, AbsoluteAxis};
use futures::channel::mpsc as un_mpsc;
use futures::{StreamExt, SinkExt, FutureExt};
use failure::{Error, format_err};
use config::ConfigQemuRouting;
use qapi::{qmp, Any, Command};
use qemu::Qemu;
use uinput;

pub struct RouteQmp {
    qemu: Arc<Qemu>,
}

impl RouteQmp {
    pub fn new(qemu: Arc<Qemu>) -> Self {
        RouteQmp {
            qemu,
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
            Ok(EventRef::Key(KeyEvent { key: Key::Reserved, .. })) =>
                return None, // ignore key 0 events
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

    fn convert_events<I: IntoIterator<Item=InputEvent>>(e: I) -> impl Iterator<Item=qmp::InputEvent> {
        e.into_iter().map(|ref e| Self::convert_event(e)).filter_map(|e| e)
    }

    pub fn spawn(&self, mut events: un_mpsc::Receiver<InputEvent>, mut error_sender: un_mpsc::Sender<Error>) {
        let qemu = self.qemu.clone();
        tokio::spawn(async move {
            let qmp = qemu.qmp_clone().await?;
            let mut cmd = qmp::input_send_event {
                device: Default::default(),
                head: Default::default(),
                events: Default::default(),
            };
            'outer: while let Some(event) = events.next().await {
                const THRESHOLD: usize = 0x20;
                cmd.events.clear();
                cmd.events.extend(RouteQmp::convert_events(iter::once(event)));
                while let Poll::Ready(event) = futures::poll!(events.next()) {
                    match event {
                        Some(event) =>
                            cmd.events.extend(RouteQmp::convert_events(iter::once(event))),
                        None => break 'outer,
                    }
                    if cmd.events.len() > THRESHOLD {
                        break
                    }
                }
                if !cmd.events.is_empty() {
                    qmp.execute::<_, qmp::input_send_event>(&cmd).await??;
                }
            }
            Ok(())
        }.then(move |r| async move { match r {
            Err(e) => {
                let _ = error_sender.send(e).await;
            },
            _ => (),
        } }));
    }
}

pub struct RouteUInput<F: ?Sized, D: ?Sized> {
    qemu: Arc<Qemu>,
    builder: uinput::Builder,
    create: Arc<Box<F>>,
    delete: Arc<Box<D>>,
}

impl<F: ?Sized, D: ?Sized> RouteUInput<F, D> {
    pub fn builder(&mut self) -> &mut uinput::Builder {
        &mut self.builder
    }
}

impl RouteUInput<dyn Fn(PathBuf) -> (String, qmp::object_add) + Send + Sync, dyn Fn(String) -> qmp::object_del + Send + Sync> {
    pub fn new_input_linux(qemu: Arc<Qemu>, id: String, repeat: bool) -> Self {
        Self::new(qemu, uinput::Builder::new(), Arc::new(Box::new(move |path: PathBuf| {
            let path = path.display();
            (id.clone(), qmp::object_add {
                id: id.clone(),
                qom_type: "input-linux".into(),
                props: Some(vec![
                    ("evdev".into(), Any::String(path.to_string())),
                    ("repeat".into(), Any::Bool(repeat)),
                ].into_iter().collect()),
            })
        }) as Box<_>), Arc::new(Box::new(|id| {
            qmp::object_del {
                id,
            }
        }) as Box<_>))
    }
}

impl RouteUInput<dyn Fn(PathBuf) -> (String, qmp::device_add) + Send + Sync, dyn Fn(String) -> qmp::device_del + Send + Sync> {
    pub fn new_virtio_host(qemu: Arc<Qemu>, id: String, bus: Option<String>) -> Self {
        Self::new(qemu, uinput::Builder::new(), Arc::new(Box::new(move |path: PathBuf| {
            // TODO: should this be virtio-input-host-pci?
            (id.clone(), qmp::device_add::new("virtio-input-host-device".into(), Some(id.clone()), bus.clone(), vec![
                ("evdev".into(), Any::String(path.display().to_string())),
            ]))
        }) as Box<_>), Arc::new(Box::new(|id| {
            qmp::device_del {
                id,
            }
        }) as Box<_>))
    }
}

impl<C: Command + Send + Sync + 'static, CD: Command + Send + Sync + 'static> RouteUInput<dyn Fn(PathBuf) -> (String, C) + Send + Sync, dyn Fn(String) -> CD + Send + Sync> {
    fn new(qemu: Arc<Qemu>, builder: uinput::Builder, create: Arc<Box<dyn Fn(PathBuf) -> (String, C) + Send + Sync>>, delete: Arc<Box<dyn Fn(String) -> CD + Send + Sync>>) -> Self {
        RouteUInput {
            qemu,
            builder,
            create,
            delete,
        }
    }

    pub fn spawn(&self, mut events: un_mpsc::Receiver<InputEvent>, mut error_sender: un_mpsc::Sender<Error>) {
        let create = self.create.clone();
        let delete = self.delete.clone();
        let qemu = self.qemu.clone();
        let uinput = self.builder.create();
        tokio::spawn(async move {
            let uinput = uinput?;
            let path = uinput.path().to_owned();
            let mut uinput = uinput.to_sink()?;
            let (id, create) = create(path);
            let res = {
                let qemu = qemu.clone();
                async move {
                    qemu.execute_qmp(create).await?;
                    while let Some(e) = events.next().await {
                        uinput.send(e).await
                            .map_err(|e| format_err!("uinput write failed: {:?}", e))?;
                    }
                    Ok(())
                }
            }.await;
            let qres = qemu.execute_qmp(delete(id)).await
                .map(drop).map_err(From::from);
            res.and_then(move |()| qres)
        }.then(move |r: Result<(), Error>| async move { match r {
            Err(e) => {
                let _ = error_sender.send(e).await;
            },
            _ => (),
        } }));
    }
}

pub enum Route {
    InputLinux(RouteUInput<dyn Fn(PathBuf) -> (String, qmp::object_add) + Send + Sync, dyn Fn(String) -> qmp::object_del + Send + Sync>),
    VirtioHost(RouteUInput<dyn Fn(PathBuf) -> (String, qmp::device_add) + Send + Sync, dyn Fn(String) -> qmp::device_del + Send + Sync>),
    Qmp(RouteQmp),
    //Spice(RouteInputSpice),
}

impl Route {
    pub fn new(routing: ConfigQemuRouting, qemu: Arc<Qemu>, id: String, bus: Option<String>, repeat: bool) -> Self {
        match routing {
            ConfigQemuRouting::InputLinux => Route::InputLinux(RouteUInput::new_input_linux(qemu, id, repeat)),
            ConfigQemuRouting::VirtioHost => Route::VirtioHost(RouteUInput::new_virtio_host(qemu, id, bus)),
            ConfigQemuRouting::Qmp => Route::Qmp(RouteQmp::new(qemu)),
            ConfigQemuRouting::Spice => unimplemented!("SPICE routing"),
        }
    }

    pub fn builder(&mut self) -> Option<&mut uinput::Builder> {
        match *self {
            Route::InputLinux(ref mut uinput) => Some(uinput.builder()),
            Route::VirtioHost(ref mut uinput) => Some(uinput.builder()),
            Route::Qmp(..) => None,
        }
    }

    pub fn spawn(self, error_sender: un_mpsc::Sender<Error>) -> un_mpsc::Sender<InputEvent> {
        let (sender, events) = un_mpsc::channel(crate::EVENT_BUFFER);

        match self {
            Route::InputLinux(ref uinput) => uinput.spawn(events, error_sender),
            Route::VirtioHost(ref uinput) => uinput.spawn(events, error_sender),
            Route::Qmp(ref qmp) => qmp.spawn(events, error_sender),
        }

        sender
    }
}

// TODO: spice input
/*pub struct RouteInputSpice {
}*/
