use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::pin::Pin;
//use futures::{future, Stream, Future, IntoFuture};
use futures::{future, select, FutureExt, StreamExt, SinkExt, TryFutureExt};
use futures::channel::mpsc as un_mpsc;
use futures::lock::Mutex;
use failure::{Error, format_err};
use config::{ConfigEvent, ConfigGrab, ConfigGrabMode, ConfigInputEvent, ConfigQemuRouting, ConfigQemuDriver};
use qapi::qga::{guest_shutdown, GuestShutdownMode};
use qapi::qmp::{self, device_add, device_del, qom_list};
use input::{self, InputEvent, RelativeAxis, InputId};
use qemu::{Qemu, QemuStream, QemuEvents};
use crate::filter::InputEventFilter;
use crate::inputs::Inputs;
use crate::route::Route;
use crate::grab::GrabEvdev;
use crate::exec::exec;
use x::XRequest;
use crate::EVENT_BUFFER;
use log::{trace, info, error};
use tokio::pin;

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
    inputs: Arc<Pin<Box<Inputs>>>,
    grabs: Arc<Mutex<HashMap<ConfigGrabMode, GrabHandle>>>,
    x_input_filter: Arc<Mutex<InputEventFilter>>,
    xreq_sender: un_mpsc::Sender<XRequest>,
    event_sender: un_mpsc::Sender<InputEvent>,
    error_sender: un_mpsc::Sender<Error>,
    uinput_id: Arc<InputId>,
}

#[derive(Debug, Copy, Clone)]
enum InputDevice {
    Keyboard,
    Relative,
    Absolute,
}

impl Process {
    pub fn new(routing: ConfigQemuRouting, driver_keyboard: ConfigQemuDriver, driver_relative: ConfigQemuDriver, driver_absolute: ConfigQemuDriver, exit_events: Vec<config::ConfigEvent>, qemu: Arc<Qemu>, inputs: Inputs, xreq_sender: un_mpsc::Sender<XRequest>, event_sender: un_mpsc::Sender<InputEvent>, error_sender: un_mpsc::Sender<Error>) -> Self {
        Process {
            routing,
            driver_keyboard,
            driver_relative,
            driver_absolute,
            exit_events,
            qemu,
            inputs: Arc::new(Box::pin(inputs)),
            grabs: Arc::new(Mutex::new(Default::default())),
            x_input_filter: Arc::new(Mutex::new(InputEventFilter::empty())),
            xreq_sender,
            event_sender,
            error_sender,
            uinput_id: Arc::new(InputId {
                bustype: input::sys::BUS_VIRTUAL,
                vendor: 0x16c0,
                product: 0x05df,
                version: 1,
            }),
        }
    }

    pub fn x_filter(&self) -> Arc<Mutex<InputEventFilter>> {
        self.x_input_filter.clone()
    }

    fn device_id(device: InputDevice) -> &'static str {
        match device {
            InputDevice::Keyboard => "screenstub-dev-kbd",
            InputDevice::Relative => "screenstub-dev-mouse",
            InputDevice::Absolute => "screenstub-dev-mouse",
        }
    }

    fn add_device_cmd(device: InputDevice, driver: ConfigQemuDriver) -> Option<device_add> {
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
        Some(device_add::new(driver.into(), Some(id.into()), None, Vec::new()))
    }

    fn device_exists_cmd(id: &str) -> qom_list {
        let path = format!("/machine/peripheral/{}", id);
        qom_list { path }
    }

    fn device_exists_map(e: Result<Vec<qmp::ObjectPropertyInfo>, qapi::Error>) -> Result<bool, Error> {
        e.map(|_| true).or_else(|e| if let qapi::ErrorClass::DeviceNotFound = e.class { Ok(false) } else { Err(e.into()) })
    }

    async fn devices_init_cmd(qemu: Arc<Qemu>, routing: ConfigQemuRouting, device: InputDevice, driver: ConfigQemuDriver) -> Result<(), Error> {
        match routing {
            ConfigQemuRouting::VirtioHost => return Ok(()),
            _ => (),
        };

        let qmp = qemu.qmp_clone().await?;
        let mut events = &*qmp;
        let mut events = Pin::new(&mut events);

        let id = Self::device_id(device);
        let device_exists = qmp.execute(Self::device_exists_cmd(id))
            .map_err(From::from)
            .and_then(|r| future::ready(Self::device_exists_map(r)));
        pin!(device_exists);
        let device_exists = loop {
            select! {
                res = device_exists => break res?,
                e = events.next() => {
                    let _ = e.transpose()?;
                },
            }
        };
        if device_exists {
            let mut events = events.as_mut();
            let f1 = qmp.execute(device_del { id: id.into() })
                .map_err(Error::from)
                .and_then(|r| future::ready(r.map_err(From::from)))
                .map_ok(drop);
            let f2 = async move {
                while let Some(e) = events.next().await {
                    match e? {
                        qmp::Event::DEVICE_DELETED { ref data, .. } if data.device.as_ref().map(|s| &s[..]) == Some(id) => return Ok(()),
                        _ => (),
                    }
                }
                Err(format_err!("Expected DEVICE_DELETED event"))
            };

            let _ = future::try_join(f1, f2).await?;
        }
        if let Some(c) = Self::add_device_cmd(device, driver) {
            let c = qmp.execute(c)
                .map_err(Error::from)
                .and_then(|r| future::ready(r.map_err(From::from)))
                .map_ok(drop);
            pin!(c);
            loop {
                select! {
                    res = c => break res?,
                    e = events.next() => {
                        let _ = e.transpose()?;
                    },
                }
            }
        }

        Ok(())
    }

    pub async fn devices_init(&self) -> Result<(), Error> {
        let routing = self.routing;
        let driver_keyboard = self.driver_keyboard;

        Self::devices_init_cmd(self.qemu.clone(), routing, InputDevice::Keyboard, driver_keyboard).await
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
                self.grabs.try_lock().unwrap().insert(mode, GrabHandle {
                    grab: None,
                    x_filter: Default::default(),
                    is_mouse: false,
                });
                self.xreq(XRequest::Grab)
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

                        uinput.spawn(error_sender.clone())
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

                    x_filter.try_lock().unwrap().set_filter(xcore_ignore.iter().cloned());

                    grabs.try_lock().unwrap().insert(mode, GrabHandle {
                        grab: Some(grab),
                        x_filter: xcore_ignore,
                        is_mouse,
                    });

                    Self::set_is_mouse_cmd(qemu, routing, driver_relative, driver_absolute, is_mouse).await
                }.boxed()
            },
            _ => future::err(format_err!("grab {:?} unimplemented", mode)).boxed(),
        }
    }

    pub fn is_mouse(&self) -> bool {
        // TODO: no grabs doesn't necessarily mean absolute mode...
        self.grabs.try_lock().unwrap().iter().map(|(_, g)| g.is_mouse).next().unwrap_or(false)
    }

    fn ungrab(&self, grab: ConfigGrabMode) -> Pin<Box<dyn Future<Output=Result<(), Error>> + Send>> {
        match grab {
            ConfigGrabMode::XCore => {
                self.grabs.try_lock().unwrap().remove(&grab);
                self.xreq(XRequest::Ungrab)
            },
            ConfigGrabMode::Evdev => {
                if let Some(mut grab) = self.grabs.try_lock().unwrap().remove(&grab) {
                    self.x_input_filter.try_lock().unwrap().unset_filter(grab.x_filter.drain(..));
                    if grab.is_mouse {
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

    pub fn process_user_event(&self, event: &ConfigEvent) -> Pin<Box<dyn Future<Output=Result<(), Error>> + Send>> {
        trace!("process_user_event({:?})", event);
        info!("User event {:?}", event);
        match event {
            ConfigEvent::ShowHost => {
                self.inputs.show_host().boxed()
            },
            ConfigEvent::ShowGuest => {
                self.inputs.show_guest().boxed()
            },
            ConfigEvent::Exec(args) => {
                exec(args).into_future().boxed()
            },
            ConfigEvent::ToggleGrab(ref grab) => {
                let mode = grab.mode();
                if self.grabs.try_lock().unwrap().contains_key(&mode) {
                    self.ungrab(mode)
                } else {
                    self.grab(grab)
                }
            },
            ConfigEvent::ToggleShow => {
                if self.inputs.showing_guest() {
                    self.inputs.show_host().boxed()
                } else {
                    self.inputs.show_guest().boxed()
                }
            },
            ConfigEvent::Grab(grab) => self.grab(grab),
            ConfigEvent::Ungrab(grab) => self.ungrab(*grab),
            ConfigEvent::UnstickGuest => {
                // TODO: this shouldn't be necessary as a xevent
                self.xreq(XRequest::UnstickGuest)
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
                    .filter_map(|e| if matches!(e, ConfigEvent::Exit) {
                        None
                    } else {
                        Some(e)
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
