use std::collections::HashMap;
use std::cell::RefCell;
use std::rc::Rc;
use tokio_core::reactor::Handle;
use futures::{future, Future, IntoFuture};
use futures::unsync::mpsc as un_mpsc;
use failure::Error;
use config::{ConfigEvent, ConfigGrab, ConfigGrabMode, ConfigInputEvent, ConfigQemuRouting, ConfigQemuDriver};
use qapi::qga::{guest_shutdown, GuestShutdownMode};
use qapi::qmp::{device_add, device_del, qom_list};
use qapi;
use input::{self, InputEvent, RelativeAxis, InputId};
use qemu::Qemu;
use filter::InputEventFilter;
use inputs::Inputs;
use route::{RouteUInput, RouteQmp};
use grab::{Grab, GrabEvdev};
use exec::exec;
use x::XRequest;
use ::EVENT_BUFFER;

pub struct GrabHandle {
    grab: Grab,
    x_filter: Vec<ConfigInputEvent>,
    is_mouse: bool,
}

pub struct Process {
    handle: Handle,
    routing: ConfigQemuRouting,
    driver_keyboard: ConfigQemuDriver,
    driver_relative: ConfigQemuDriver,
    driver_absolute: ConfigQemuDriver,
    qemu: Rc<Qemu>,
    inputs: Rc<Inputs>,
    grabs: Rc<RefCell<HashMap<ConfigGrabMode, GrabHandle>>>,
    x_input_filter: Rc<RefCell<InputEventFilter>>,
    event_sender: un_mpsc::Sender<InputEvent>,
    error_sender: un_mpsc::Sender<Error>,
    uinput_id: Rc<InputId>,
}

#[derive(Debug, Copy, Clone)]
enum InputDevice {
    Keyboard,
    Relative,
    Absolute,
}

impl Process {
    pub fn new(handle: Handle, routing: ConfigQemuRouting, driver_keyboard: ConfigQemuDriver, driver_relative: ConfigQemuDriver, driver_absolute: ConfigQemuDriver, qemu: Rc<Qemu>, inputs: Rc<Inputs>, event_sender: un_mpsc::Sender<InputEvent>, error_sender: un_mpsc::Sender<Error>) -> Self {
        Process {
            handle: handle,
            routing: routing,
            driver_keyboard: driver_keyboard,
            driver_relative: driver_relative,
            driver_absolute: driver_absolute,
            qemu: qemu,
            inputs: inputs,
            grabs: Rc::new(RefCell::new(Default::default())),
            x_input_filter: Rc::new(RefCell::new(InputEventFilter::empty())),
            event_sender: event_sender,
            error_sender: error_sender,
            uinput_id: Rc::new(InputId {
                bustype: input::sys::BUS_VIRTUAL,
                vendor: 0x16c0,
                product: 0x05df,
                version: 1,
            }),
        }
    }

    fn add_device(&self, device: InputDevice, driver: ConfigQemuDriver) -> Box<Future<Item=(), Error=Error>> {
        let driver = match (device, driver) {
            (InputDevice::Absolute, ConfigQemuDriver::Ps2) => return Box::new(future::err(format_err!("PS/2 tablet not possible"))) as Box<_>,
            (_, ConfigQemuDriver::Ps2) => return Box::new(future::ok(())) as Box<_>,
            (InputDevice::Keyboard, ConfigQemuDriver::Usb) => "usb-kbd",
            (InputDevice::Relative, ConfigQemuDriver::Usb) => "usb-mouse",
            (InputDevice::Absolute, ConfigQemuDriver::Usb) => "usb-tablet",
            (InputDevice::Keyboard, ConfigQemuDriver::Virtio) => "virtio-keyboard-pci",
            (InputDevice::Relative, ConfigQemuDriver::Virtio) => "virtio-mouse-pci",
            (InputDevice::Absolute, ConfigQemuDriver::Virtio) => "virtio-tablet-pci",
        };

        let id = Self::device_id(device);
        Box::new(self.qemu.execute_qmp(&self.handle, device_add::new(driver.into(), Some(id.into()), None, Vec::new()))
            .map(drop)) as Box<_>
    }

    fn del_device(&self, device: InputDevice) -> Box<Future<Item=(), Error=Error>> {
        let id = Self::device_id(device);

        Box::new(self.qemu.execute_qmp(&self.handle, device_del { id: id.into() })
            .map(drop)) as Box<_>
    }

    fn device_exists(&self, id: &str) -> Box<Future<Item=bool, Error=Error>> {
        let path = format!("/machine/peripheral/{}", id);
        Box::new(self.qemu.execute_qmp(&self.handle, qom_list { path: path }).map(|_| true)
            .or_else(|e| match e.downcast::<qapi::Error>() {
                Ok(e) => if let qapi::ErrorClass::DeviceNotFound = e.class { Ok(false) } else { Err(e.into()) },
                Err(e) => Err(e),
            })) as Box<_>
    }

    fn device_id(device: InputDevice) -> &'static str {
        match device {
            InputDevice::Keyboard => "screenstub-kbd",
            InputDevice::Relative => "screenstub-rel",
            InputDevice::Absolute => "screenstub-abs",
        }
    }

    fn set_is_mouse(&self, is_mouse: bool) -> Box<Future<Item=(), Error=Error>> {
        unimplemented!()
    }

    fn grab(&self, grab: &ConfigGrab) -> ProcessedUserEvent {
        let mode = grab.mode();

        match *grab {
            ConfigGrab::XCore => {
                self.grabs.borrow_mut().insert(mode, GrabHandle {
                    grab: Grab::XCore,
                    x_filter: Default::default(),
                    is_mouse: false,
                });
                xreq(XRequest::Grab)
            },
            ConfigGrab::Evdev { exclusive, ref new_device_name, ref xcore_ignore, ref evdev_ignore, ref devices } => {
                future::result(GrabEvdev::new(&self.handle, devices, evdev_ignore.iter().cloned()))
                    .and_then({
                        let qemu = self.qemu.clone();
                        let handle = self.handle.clone();
                        let grabs = self.grabs.clone();
                        let x_filter = self.x_input_filter.clone();
                        let xcore_ignore = xcore_ignore.clone();
                        let devname = new_device_name.clone();
                        let error_sender = self.error_sender.clone();
                        let event_sender = if new_device_name.is_some() { None } else { Some(self.event_sender.clone()) };
                        let routing = self.routing;
                        let uinput_id = self.uinput_id.clone();
                        move |grab| -> Result<_, Error> {
                            let event_sender = if let Some(devname) = devname {
                                enum Trio<A, B, C> {
                                    A(A),
                                    B(B),
                                    C(C),
                                }

                                let id = format!("screenstub-uinput-{}", devname);
                                let repeat = false;
                                let bus = None;
                                let mut uinput = match routing {
                                    ConfigQemuRouting::InputLinux => Trio::A(RouteUInput::new_input_linux(qemu, id, repeat)),
                                    ConfigQemuRouting::VirtioHost => Trio::B(RouteUInput::new_virtio_host(qemu, id, bus)),
                                    ConfigQemuRouting::Qmp => Trio::C(RouteQmp::new(qemu)),
                                };

                                let mut builder = match uinput {
                                    Trio::A(ref mut uinput) => Some(uinput.builder()),
                                    Trio::B(ref mut uinput) => Some(uinput.builder()),
                                    Trio::C(..) => None,
                                };

                                if let Some(builder) = builder.as_mut() {
                                    builder.name(&devname);
                                    builder.id(&uinput_id);
                                }

                                for evdev in grab.evdevs() {
                                    if let Some(builder) = builder.as_mut() {
                                        builder.from_evdev(&evdev)?;
                                    }

                                    if exclusive {
                                        evdev.grab(true)?;
                                    }
                                }

                                let (send, recv) = un_mpsc::channel(EVENT_BUFFER);

                                match uinput {
                                    Trio::A(uinput) => uinput.spawn(&handle, recv, error_sender.clone()),
                                    Trio::B(uinput) => uinput.spawn(&handle, recv, error_sender.clone()),
                                    Trio::C(qmp) => qmp.spawn(&handle, recv, error_sender.clone()),
                                }

                                send
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

                            grab.spawn(&handle, event_sender, error_sender);

                            x_filter.borrow_mut().set_filter(xcore_ignore.iter().cloned());

                            grabs.borrow_mut().insert(mode, GrabHandle {
                                grab: grab.into(),
                                x_filter: xcore_ignore,
                                is_mouse: is_mouse,
                            });

                            // TODO: qemu.set_is_mouse!!

                            Ok(())
                        }
                    }).into()
            },
            _ => future::err(format_err!("grab {:?} unimplemented", mode)).into(),
        }
    }

    fn ungrab(&self, grab: ConfigGrabMode) -> ProcessedUserEvent {
        match grab {
            ConfigGrabMode::XCore => {
                self.grabs.borrow_mut().remove(&grab);
                xreq(XRequest::Ungrab)
            },
            ConfigGrabMode::Evdev => {
                if let Some(grab) = self.grabs.borrow_mut().remove(&grab) {
                    self.x_input_filter.borrow_mut().unset_filter(grab.x_filter.into_iter());
                    if grab.is_mouse {
                        self.set_is_mouse(false).into()
                    } else {
                        future::ok(()).into()
                    }
                } else {
                    info!("requested non-existent grab");
                    future::ok(()).into()
                }
            },
            grab => future::err(format_err!("ungrab {:?} unimplemented", grab)).into(),
        }
    }

    pub fn process_user_event(&self, event: &ConfigEvent) -> Vec<ProcessedUserEvent> {
        trace!("process_user_event({:?})", event);
        info!("User event {:?}", event);
        match *event {
            ConfigEvent::ShowHost => {
                user(self.inputs.show_host())
            },
            ConfigEvent::ShowGuest => {
                user(self.inputs.show_guest())
            },
            ConfigEvent::Exec(ref args) => {
                vec![exec(&self.handle, args).into()]
            }
            ConfigEvent::ToggleGrab(ref grab) => {
                let mode = grab.mode();
                if self.grabs.borrow().contains_key(&mode) {
                    vec![self.ungrab(mode)]
                } else {
                    vec![self.grab(grab)]
                }
            }
            ConfigEvent::ToggleShow => {
                if self.inputs.showing_guest() {
                    user(self.inputs.show_host())
                } else {
                    user(self.inputs.show_guest())
                }
            },
            ConfigEvent::Grab(ref grab) => vec![self.grab(grab)],
            ConfigEvent::Ungrab(grab) => vec![self.ungrab(grab)],
            ConfigEvent::UnstickGuest => {
                vec![xreq(XRequest::UnstickGuest)] // TODO: this shouldn't be necessary as a xevent
            }
            ConfigEvent::UnstickHost => {
                vec![xreq(XRequest::UnstickHost)]
            }
            ConfigEvent::Shutdown => {
                user(self.qemu.guest_shutdown(&self.handle, guest_shutdown { mode: Some(GuestShutdownMode::Powerdown) }))
            },
            ConfigEvent::Reboot => {
                user(self.qemu.guest_shutdown(&self.handle, guest_shutdown { mode: Some(GuestShutdownMode::Reboot) }))
            },
            ConfigEvent::Exit => {
                vec![xreq(XRequest::Quit)]
            }
        }
    }
}

pub enum ProcessedUserEvent {
    UserEvent(Box<Future<Item=(), Error=Error>>),
    XRequest(XRequest),
}

fn xreq(r: XRequest) -> ProcessedUserEvent {
    ProcessedUserEvent::XRequest(r)
}

fn user<F: IntoFuture<Item=(), Error=Error> + 'static>(f: F) -> Vec<ProcessedUserEvent> {
    vec![f.into()]
}

impl<F: IntoFuture<Item=(), Error=Error> + 'static> From<F> for ProcessedUserEvent {
    fn from(f: F) -> Self {
        ProcessedUserEvent::UserEvent(Box::new(f.into_future()) as Box<_>)
    }
}
