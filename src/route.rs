use std::sync::Arc;
use std::path::Path;
use std::task::Poll;
use std::sync::Once;
use std::pin::Pin;
use std::iter;
use tokio::time::{Duration, Instant};
use input::{InputEvent, EventRef, KeyEvent, Key, RelativeAxis, AbsoluteAxis};
use futures::channel::mpsc;
use futures::{StreamExt, SinkExt, Future, FutureExt, TryFutureExt};
use anyhow::{Error, Context};
use config::ConfigQemuRouting;
use config::keymap::Keymaps;
use qapi::{qmp, Any};
use qemu::Qemu;
use uinput;
use log::warn;
use crate::spawner::Spawner;

pub struct RouteQmp {
    qemu: Arc<Qemu>,
    qkeycodes: Arc<[u8]>,
}

impl RouteQmp {
    pub fn new(qemu: Arc<Qemu>) -> Self {
        let qkeycodes = unsafe {
            static mut QKEYCODES: Option<Arc<[u8]>> = None;
            static QKEYCODES_ONCE: Once = Once::new();

            QKEYCODES_ONCE.call_once(|| {
                QKEYCODES = Some(Keymaps::from_csv().qnum_keycodes().into());
            });
            QKEYCODES.as_ref().unwrap().clone()
        };
        RouteQmp {
            qemu,
            qkeycodes,
        }
    }

    fn convert_event(e: &InputEvent, qkeycodes: &[u8]) -> Option<qmp::InputEvent> {
        Some(match EventRef::new(e) {
            Ok(EventRef::Key(ref key)) if key.key.is_button() => qmp::InputEvent::btn {
                data: qmp::InputBtnEvent {
                    down: key.value.is_pressed(),
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
            Ok(EventRef::Key(ref key)) => match qkeycodes.get(key.key as usize) {
                Some(&qnum) => qmp::InputEvent::key {
                    data: qmp::InputKeyEvent {
                        down: key.value.is_pressed(),
                        key: qmp::KeyValue::number { data: qnum as _ },
                    },
                },
                None => return None,
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

    fn convert_events<'a, I: IntoIterator<Item=InputEvent> + 'a>(e: I, qkeycodes: &'a [u8]) -> impl Iterator<Item=qmp::InputEvent> + 'a {
        e.into_iter().map(move |ref e| Self::convert_event(e, qkeycodes)).filter_map(|e| e)
    }

    pub fn spawn(&self, spawner: &Spawner, mut events: mpsc::Receiver<InputEvent>, mut error_sender: mpsc::Sender<Error>) {
        let qemu = self.qemu.clone();
        let qkeycodes = self.qkeycodes.clone();
        spawner.spawn(async move {
            let qmp = qemu.connect_qmp().await?;
            let mut cmd = qmp::input_send_event {
                device: Default::default(),
                head: Default::default(),
                events: Default::default(),
            };
            'outer: while let Some(event) = events.next().await {
                const THRESHOLD: usize = 0x20;
                cmd.events.clear();
                cmd.events.extend(RouteQmp::convert_events(iter::once(event), &qkeycodes));
                while let Poll::Ready(event) = futures::poll!(events.next()) {
                    match event {
                        Some(event) =>
                            cmd.events.extend(RouteQmp::convert_events(iter::once(event), &qkeycodes)),
                        None => break 'outer,
                    }
                    if cmd.events.len() > THRESHOLD {
                        break
                    }
                }
                if !cmd.events.is_empty() {
                    match qmp.execute(&cmd).await {
                        Ok(_) => (),
                        Err(qapi::ExecuteError::Qapi(e @ qapi::Error { class: qapi::ErrorClass::GenericError, .. })) =>
                            warn!("QMP input routing error: {:?}", e),
                        Err(e) => return Err(e.into()),
                    }
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

pub struct RouteUInput<U> {
    qemu: Arc<Qemu>,
    builder: uinput::Builder,
    commands: Arc<U>,
}

impl<U> RouteUInput<U> {
    pub fn builder(&mut self) -> &mut uinput::Builder {
        &mut self.builder
    }
}

impl RouteUInput<RouteUInputInputLinux> {
    pub fn new_input_linux(qemu: Arc<Qemu>, id: String, repeat: bool) -> Self {
        Self::new(qemu, uinput::Builder::new(), RouteUInputInputLinux {
            id,
            repeat,
        })
    }
}

impl RouteUInput<RouteUInputVirtio> {
    pub fn new_virtio_host(qemu: Arc<Qemu>, id: String, bus: Option<String>) -> Self {
        Self::new(qemu, uinput::Builder::new(), RouteUInputVirtio {
            id,
            bus,
        })
    }
}

pub trait UInputCommands: Send + Sync + 'static {
    fn command_create(&self, qemu: &Arc<Qemu>, path: &Path) -> Pin<Box<dyn Future<Output=Result<(), Error>> + Send>>;
    fn command_delete(&self, qemu: &Arc<Qemu>) -> Pin<Box<dyn Future<Output=Result<(), Error>> + Send>>;
}

pub struct RouteUInputVirtio {
    id: String,
    bus: Option<String>,
}

impl UInputCommands for RouteUInputVirtio {
    fn command_create(&self, qemu: &Arc<Qemu>, path: &Path) -> Pin<Box<dyn Future<Output=Result<(), Error>> + Send>> {
        let command = qmp::device_add::new("virtio-input-host-pci", Some(self.id.clone()), self.bus.clone(), vec![
            ("evdev".into(), Any::String(path.display().to_string())),
            ("multifunction".into(), Any::Bool(true)),
        ]);
        let deadline = Instant::now() + Duration::from_millis(512); // HACK: wait for udev to see device and change permissions
        let qemu = qemu.clone();
        async move {
            qemu.device_add(command, deadline).await
        }.boxed()
    }

    fn command_delete(&self, qemu: &Arc<Qemu>) -> Pin<Box<dyn Future<Output=Result<(), Error>> + Send>> {
        let command = qmp::device_del {
            id: self.id.clone(),
        };
        let qemu = qemu.clone();
        async move {
            qemu.execute_qmp(command).map_ok(drop).await
        }.boxed()
    }
}

pub struct RouteUInputInputLinux {
    id: String,
    repeat: bool,
}

impl UInputCommands for RouteUInputInputLinux {
    fn command_create(&self, qemu: &Arc<Qemu>, path: &Path) -> Pin<Box<dyn Future<Output=Result<(), Error>> + Send>> {
        let path = path.display();
        let command = qmp::object_add::from(qmp::ObjectOptions::input_linux {
            id: self.id.clone(),
            input_linux: qmp::InputLinuxProperties {
                evdev: path.to_string(),
                repeat: Some(self.repeat),
                grab_all: None,
                grab_toggle: None,
            },
        });
        let delete_command = qmp::object_del {
            id: self.id.clone(),
        };
        let qemu = qemu.clone();
        async move {
            if qemu.execute_qmp(delete_command).await.is_ok() {
                tokio::time::sleep(Duration::from_millis(512)).await;
            }
            qemu.execute_qmp(command).map_ok(drop).await
        }.boxed()
    }

    fn command_delete(&self, qemu: &Arc<Qemu>) -> Pin<Box<dyn Future<Output=Result<(), Error>> + Send>> {
        let command = qmp::object_del {
            id: self.id.clone(),
        };
        let qemu = qemu.clone();
        async move {
            qemu.execute_qmp(command).map_ok(drop).await
        }.boxed()
    }
}

impl<U> RouteUInput<U> {
    fn new(qemu: Arc<Qemu>, builder: uinput::Builder, commands: U) -> Self {
        RouteUInput {
            qemu,
            builder,
            commands: Arc::new(commands),
        }
    }
}

impl<U: UInputCommands> RouteUInput<U> {
    pub fn spawn(&self, spawner: &Spawner, mut events: mpsc::Receiver<InputEvent>, mut error_sender: mpsc::Sender<Error>) {
        let qemu = self.qemu.clone();
        let uinput = self.builder.create();
        let commands = self.commands.clone();
        spawner.spawn(async move {
            let uinput = uinput?;
            let path = uinput.path().to_owned();
            let mut uinput = uinput.to_sink()?;
            commands.command_create(&qemu, &path).await?;
            let res = async move {
                while let Some(e) = events.next().await {
                    uinput.send(e).await
                        .context("uinput write failed")?;
                }
                Ok(())
            }.await;
            let qres = commands.command_delete(&qemu).await
                .map_err(From::from);
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
    InputLinux(RouteUInput<RouteUInputInputLinux>),
    VirtioHost(RouteUInput<RouteUInputVirtio>),
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

    pub fn spawn(self, spawner: &Spawner, error_sender: mpsc::Sender<Error>) -> mpsc::Sender<InputEvent> {
        let (sender, events) = mpsc::channel(crate::EVENT_BUFFER);

        match self {
            Route::InputLinux(ref uinput) => uinput.spawn(spawner, events, error_sender),
            Route::VirtioHost(ref uinput) => uinput.spawn(spawner, events, error_sender),
            Route::Qmp(ref qmp) => qmp.spawn(spawner, events, error_sender),
        }

        sender
    }
}

// TODO: spice input
/*pub struct RouteInputSpice {
}*/
