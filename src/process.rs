use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::pin::Pin;
//use futures::{future, Stream, Future, IntoFuture};
use futures::{future, FutureExt, SinkExt, TryFutureExt};
use futures::channel::mpsc as un_mpsc;
use std::sync::Mutex;
use failure::{Error, format_err};
use config::{ConfigEvent, ConfigGrab, ConfigGrabMode, ConfigInputEvent, ConfigQemuRouting, ConfigQemuDriver};
use qapi::qga::{guest_shutdown, GuestShutdownMode};
use input::{self, InputEvent, RelativeAxis, InputId};
use qemu::Qemu;
use crate::filter::InputEventFilter;
use crate::sources::Sources;
use crate::route::Route;
use crate::grab::GrabEvdev;
use crate::exec::exec;
use x::XRequest;
use crate::Events;
use crate::spawner::Spawner;
use log::{trace, info, error};

pub struct GrabHandle {
    grab: Option<future::AbortHandle>,
    x_filter: Vec<ConfigInputEvent>,
    is_mouse: bool,
}

impl Drop for GrabHandle {
    fn drop(&mut self) {
        if let Some(grab) = self.grab.take() {
            grab.abort();
        }
    }
}

pub struct Process {
    routing: ConfigQemuRouting,
    driver_keyboard: ConfigQemuDriver,
    driver_relative: ConfigQemuDriver,
    driver_absolute: ConfigQemuDriver,
    exit_events: Vec<config::ConfigEvent>,
    qemu: Arc<Qemu>,
    events: Arc<Events>,
    sources: Arc<Pin<Box<Sources>>>,
    grabs: Arc<Mutex<HashMap<ConfigGrabMode, GrabHandle>>>,
    x_input_filter: Arc<InputEventFilter>,
    xreq_sender: un_mpsc::Sender<XRequest>,
    event_sender: un_mpsc::Sender<InputEvent>,
    error_sender: un_mpsc::Sender<Error>,
    uinput_id: Arc<InputId>,
    spawner: Arc<Spawner>,
}

#[derive(Debug, Copy, Clone)]
enum InputDevice {
    Keyboard,
    Relative,
    Absolute,
}

impl Process {
    pub fn new(routing: ConfigQemuRouting, driver_keyboard: ConfigQemuDriver, driver_relative: ConfigQemuDriver, driver_absolute: ConfigQemuDriver, exit_events: Vec<config::ConfigEvent>, qemu: Arc<Qemu>, events: Arc<Events>, sources: Sources, xreq_sender: un_mpsc::Sender<XRequest>, event_sender: un_mpsc::Sender<InputEvent>, error_sender: un_mpsc::Sender<Error>, spawner: Arc<Spawner>) -> Self {
        Process {
            routing,
            driver_keyboard,
            driver_relative,
            driver_absolute,
            exit_events,
            qemu,
            events,
            sources: Arc::new(Box::pin(sources)),
            grabs: Arc::new(Mutex::new(Default::default())),
            x_input_filter: Arc::new(InputEventFilter::empty()),
            xreq_sender,
            event_sender,
            error_sender,
            uinput_id: Arc::new(InputId {
                bustype: input::sys::BUS_VIRTUAL,
                vendor: 0x16c0,
                product: 0x05df,
                version: 1,
            }),
            spawner,
        }
    }

    pub fn x_filter(&self) -> Arc<InputEventFilter> {
        self.x_input_filter.clone()
    }

    fn device_id(device: InputDevice) -> &'static str {
        match device {
            InputDevice::Keyboard => "screenstub-dev-kbd",
            InputDevice::Relative => "screenstub-dev-mouse",
            InputDevice::Absolute => "screenstub-dev-mouse",
        }
    }

    fn add_device_cmd(device: InputDevice, driver: ConfigQemuDriver) -> Option<qapi::qmp::device_add> {
        let driver = match (device, driver) {
            (InputDevice::Absolute, ConfigQemuDriver::Ps2) => panic!("PS/2 tablet not possible"),
            (_, ConfigQemuDriver::Ps2) => return None,
            (InputDevice::Keyboard, ConfigQemuDriver::Usb) => "usb-kbd",
            (InputDevice::Relative, ConfigQemuDriver::Usb) => "usb-mouse",
            (InputDevice::Absolute, ConfigQemuDriver::Usb) => "usb-tablet",
            (InputDevice::Keyboard, ConfigQemuDriver::Virtio) => "virtio-keyboard-pci",
            (InputDevice::Relative, ConfigQemuDriver::Virtio) => "virtio-mouse-pci",
            (InputDevice::Absolute, ConfigQemuDriver::Virtio) => "virtio-tablet-pci",
        };

        let id = Self::device_id(device);
        Some(qapi::qmp::device_add::new(driver, Some(id.into()), None, Vec::new()))
    }

    async fn devices_init_cmd(qemu: Arc<Qemu>, routing: ConfigQemuRouting, device: InputDevice, driver: ConfigQemuDriver) -> Result<(), Error> {
        match routing {
            ConfigQemuRouting::VirtioHost => return Ok(()),
            _ => (),
        };

        if let Some(cmd) = Self::add_device_cmd(device, driver) {
            qemu.device_add(cmd, tokio::time::Instant::now()).await
        } else {
            Ok(())
        }
    }

    pub async fn devices_init(&self) -> Result<(), Error> {
        Self::devices_init_cmd(self.qemu.clone(), self.routing, InputDevice::Keyboard, self.driver_keyboard).await?;
        self.set_is_mouse(false).await?; // TODO: config option to start up in relative mode instead

        Ok(())
    }

    async fn set_is_mouse_cmd(qemu: Arc<Qemu>, routing: ConfigQemuRouting, driver_relative: ConfigQemuDriver, driver_absolute: ConfigQemuDriver, is_mouse: bool) -> Result<(), Error> {
        let (device, driver) = if is_mouse {
            (InputDevice::Relative, driver_relative)
        } else {
            (InputDevice::Absolute, driver_absolute)
        };

        Self::devices_init_cmd(qemu, routing, device, driver).await
    }

    pub fn set_is_mouse(&self, is_mouse: bool) -> impl Future<Output=Result<(), Error>> {
        Self::set_is_mouse_cmd(self.qemu.clone(), self.routing, self.driver_relative, self.driver_absolute, is_mouse)
    }

    fn grab(&self, grab: &ConfigGrab) -> Pin<Box<dyn Future<Output=Result<(), Error>> + Send>> {
        let mode = grab.mode();

        match *grab {
            ConfigGrab::XCore => {
                self.grabs.lock().unwrap().insert(mode, GrabHandle {
                    grab: None,
                    x_filter: Default::default(),
                    is_mouse: false,
                });
                self.xreq(XRequest::Grab {
                    xcore: true,
                    motion: false,
                })
            },
            ConfigGrab::XInput => {
                let qemu = self.qemu.clone();
                let routing = self.routing;
                let driver_relative = self.driver_relative;
                let driver_absolute = self.driver_absolute;
                let is_mouse = true;
                let prev_is_mouse = self.is_mouse();

                self.grabs.lock().unwrap().insert(mode, GrabHandle {
                    grab: None,
                    x_filter: Default::default(),
                    is_mouse,
                });

                let grab = self.xreq(XRequest::Grab {
                    xcore: true,
                    motion: true,
                });
                async move {
                    grab.await?;

                    if is_mouse && !prev_is_mouse {
                        Self::set_is_mouse_cmd(qemu, routing, driver_relative, driver_absolute, is_mouse).await?;
                    }

                    Ok(())
                }.boxed()
            },
            ConfigGrab::Evdev { exclusive, ref new_device_name, ref xcore_ignore, ref evdev_ignore, ref devices } => {
                let qemu = self.qemu.clone();
                let grabs = self.grabs.clone();
                let x_filter = self.x_input_filter.clone();
                let xcore_ignore = xcore_ignore.clone();
                let devname = new_device_name.clone();
                let error_sender = self.error_sender.clone();
                let event_sender = if new_device_name.is_some() { None } else { Some(self.event_sender.clone()) };
                let routing = self.routing;
                let uinput_id = self.uinput_id.clone();
                let driver_relative = self.driver_relative;
                let driver_absolute = self.driver_absolute;
                let prev_is_mouse = self.is_mouse();
                let spawner = self.spawner.clone();
                let grab = GrabEvdev::new(devices, evdev_ignore.iter().cloned());

                async move {
                    let grab = grab?;
                    let event_sender = if let Some(devname) = devname {
                        let id = format!("screenstub-uinput-{}", devname);
                        let repeat = false;
                        let bus = None;
                        let qemu = qemu.clone();
                        let mut uinput = Route::new(routing, qemu, id, bus, repeat);

                        let mut builder = uinput.builder();

                        if let Some(builder) = builder.as_mut() {
                            builder.name(&devname);
                            builder.id(&uinput_id);
                        }

                        for evdev in grab.evdevs() {
                            if let Some(builder) = builder.as_mut() {
                                builder.from_evdev(&evdev)?;
                            }
                        }

                        if exclusive {
                            grab.grab(true)?;
                        }

                        uinput.spawn(&spawner, error_sender.clone())
                    } else {
                        event_sender.unwrap()
                    };

                    let mut is_mouse = false;
                    for evdev in grab.evdevs() {
                        let rel = evdev.relative_bits()?;
                        if rel.get(RelativeAxis::X) || rel.get(RelativeAxis::Y) {
                            is_mouse = true;
                            break
                        }
                    }

                    let grab = grab.spawn(event_sender, error_sender);

                    x_filter.set_filter(xcore_ignore.iter().cloned());

                    grabs.lock().unwrap().insert(mode, GrabHandle {
                        grab: Some(grab),
                        x_filter: xcore_ignore,
                        is_mouse,
                    });

                    if is_mouse && !prev_is_mouse {
                        Self::set_is_mouse_cmd(qemu, routing, driver_relative, driver_absolute, is_mouse).await?;
                    }
                    Ok(())
                }.boxed()
            },
            _ => future::err(format_err!("grab {:?} unimplemented", mode)).boxed(),
        }
    }

    pub fn is_mouse(&self) -> bool {
        // TODO: no grabs doesn't necessarily mean absolute mode...
        self.grabs.lock().unwrap().iter().any(|(_, g)| g.is_mouse)
    }

    fn ungrab(&self, grab: ConfigGrabMode) -> Pin<Box<dyn Future<Output=Result<(), Error>> + Send>> {
        match grab {
            ConfigGrabMode::XCore | ConfigGrabMode::XInput => {
                let ungrab = self.xreq(XRequest::Ungrab);
                let grab = self.grabs.lock().unwrap().remove(&grab);
                if let Some(grab) = grab {
                    if grab.is_mouse && !self.is_mouse() {
                        let set = self.set_is_mouse(false);
                        async move {
                            set.await?;
                            ungrab.await
                        }.boxed()
                    } else {
                        ungrab
                    }
                } else {
                    ungrab
                }
            },
            ConfigGrabMode::Evdev => {
                let grab = self.grabs.lock().unwrap().remove(&grab);
                if let Some(mut grab) = grab {
                    self.x_input_filter.unset_filter(grab.x_filter.drain(..));
                    if grab.is_mouse && !self.is_mouse() {
                        self.set_is_mouse(false).boxed()
                    } else {
                        future::ok(()).boxed()
                    }
                } else {
                    info!("requested non-existent grab");
                    future::ok(()).boxed()
                }
            },
            grab => future::err(format_err!("ungrab {:?} unimplemented", grab)).boxed(),
        }
    }

    fn xreq(&self, req: XRequest) -> Pin<Box<dyn Future<Output=Result<(), Error>> + Send>> {
        let mut xreq_sender = self.xreq_sender.clone();
        async move {
            xreq_sender.send(req)
                .map_err(From::from).await
        }.boxed()
    }

    fn map_exec_arg<S: AsRef<str>>(s: S) -> Result<String, Error> {
        // TODO: variable substitution or something
        Ok(s.as_ref().into())
    }

    pub fn process_user_event(&self, event: &ConfigEvent) -> Pin<Box<dyn Future<Output=Result<(), Error>> + Send>> {
        trace!("process_user_event({:?})", event);
        info!("User event {:?}", event);
        match event {
            ConfigEvent::Exec(args) => {
                let args = args.iter()
                    .map(|i| Self::map_exec_arg(i))
                    .collect::<Result<Vec<_>, Error>>();
                match args {
                    Err(e) => future::ready(Err(e)).boxed(),
                    Ok(args) => exec(args).into_future().boxed(),
                }
            },
            ConfigEvent::GuestExec(args) => {
                let args = args.iter()
                    .map(|i| Self::map_exec_arg(i))
                    .collect::<Result<Vec<_>, Error>>();
                match args {
                    Err(e) => future::ready(Err(e)).boxed(),
                    Ok(args) => self.qemu.guest_exec(args).into_future().map_ok(drop).boxed(),
                }
            },
            ConfigEvent::GuestWait =>
                self.qemu.guest_wait().boxed(),
            ConfigEvent::ShowHost => {
                self.sources.show_host().boxed()
            },
            ConfigEvent::ShowGuest => {
                self.sources.show_guest().boxed()
            },
            ConfigEvent::ToggleShow => {
                if self.sources.showing_guest().unwrap_or_default() {
                    self.sources.show_host().boxed()
                } else {
                    self.sources.show_guest().boxed()
                }
            },
            ConfigEvent::ToggleGrab(ref grab) => {
                let mode = grab.mode();
                if self.grabs.lock().unwrap().contains_key(&mode) {
                    self.ungrab(mode)
                } else {
                    self.grab(grab)
                }
            },
            ConfigEvent::Grab(grab) => self.grab(grab),
            ConfigEvent::Ungrab(grab) => self.ungrab(*grab),
            ConfigEvent::UnstickGuest => {
                let mut event_sender = self.event_sender.clone();
                let events = self.events.clone();
                async move {
                    for e in events.unstick_guest() {
                        let _ = event_sender.send(e).await;
                    }

                    Ok(())
                }.boxed()
            },
            ConfigEvent::UnstickHost => {
                self.xreq(XRequest::UnstickHost)
            },
            ConfigEvent::Shutdown => {
                self.qemu.guest_shutdown(guest_shutdown { mode: Some(GuestShutdownMode::Powerdown) }).boxed()
            },
            ConfigEvent::Reboot => {
                self.qemu.guest_shutdown(guest_shutdown { mode: Some(GuestShutdownMode::Reboot) }).boxed()
            },
            ConfigEvent::Exit => {
                let exit_events: Vec<_> = self.exit_events.iter()
                    .filter_map(|e| match e {
                        ConfigEvent::Exit => None,
                        e => Some(e),
                    }).map(|e| self.process_user_event(e))
                    .chain(std::iter::once(self.xreq(XRequest::Quit)))
                    .collect();
                async move {
                    for e in exit_events {
                        if let Err(e) = e.await {
                            error!("Failed to run exit event: {} {:?}", e, e);
                        }
                    }
                    Ok(())
                }.boxed()
            }
        }
    }
}
