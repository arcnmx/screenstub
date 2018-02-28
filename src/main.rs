#[macro_use]
extern crate log;
extern crate env_logger;
extern crate futures;
extern crate futures_cpupool;
#[macro_use]
extern crate failure;
extern crate input_linux as input;
extern crate screenstub_uinput as uinput;
extern crate screenstub_config as config;
extern crate screenstub_event as event;
extern crate screenstub_ddc as ddc;
extern crate screenstub_x as x;
extern crate tokio_unzip;
extern crate tokio_timer;
extern crate tokio_core;
extern crate tokio_process;
extern crate serde_yaml;
extern crate clap;

use std::collections::HashMap;
use std::process::{exit, Command, ExitStatus};
use std::thread::spawn;
use std::cell::{Cell, RefCell};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use std::ffi::{OsStr, OsString};
use std::rc::Rc;
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use tokio_core::reactor::{Core, Handle};
use tokio_unzip::StreamUnzipExt;
use tokio_process::CommandExt;
use tokio_timer::Timer;
use clap::{Arg, App, SubCommand, AppSettings};
use futures::sync::mpsc;
use futures::unsync::mpsc as un_mpsc;
use futures::{Future, Stream, Sink, IntoFuture, stream, future};
use futures_cpupool::CpuPool;
use failure::Error;
use input::InputId;
use config::{
    Config, ConfigEvent, ConfigGrab, ConfigGrabMode,
    ConfigDdc, ConfigDdcHost, ConfigDdcGuest,
    ConfigQemuDriver, ConfigQemuComm,
};
use event::{Hotkey, UserEvent, ProcessedXEvent};
use ddc::{SearchDisplay, SearchInput};
#[cfg(feature = "with-ddcutil")]
use ddc::Monitor;
use x::XRequest;

fn main() {
    match main_result() {
        Ok(code) => exit(code),
        Err(e) => {
            let _ = writeln!(io::stderr(), "{:?} {}", e, e);
            exit(1);
        },
    }
}

fn main_result() -> Result<i32, Error> {
    env_logger::init();

    let app = App::new("screenstub")
        .version(env!("CARGO_PKG_VERSION"))
        .author("arcnmx")
        .about("A software KVM")
        .arg(Arg::with_name("config")
            .short("c")
            .long("config")
            .value_name("CONFIG")
            .takes_value(true)
            .help("Configuration TOML file")
        ).subcommand(SubCommand::with_name("x")
            .about("Start the KVM with a fullscreen X window")
        ).setting(AppSettings::SubcommandRequiredElseHelp);

    let matches = app.get_matches();
    let config = if let Some(config) = matches.value_of("config") {
        use std::fs::File;

        let mut f = File::open(config)?;
        serde_yaml::from_reader(f)?
    } else {
        Config::default()
    };

    match matches.subcommand() {
        ("x", Some(..)) => {
            let config = config.get(0).ok_or_else(|| format_err!("expected a screen config"))?.clone();

            let (mut xsender, xreceiver) = mpsc::channel(0x20); // TODO: up this after testing that backpressure works
            let (xreqsender, xreqreceiver) = mpsc::channel(0x08);
            let xthread = spawn(move || {
                if let Err(res) = x::XContext::xmain(xreqreceiver, &mut xsender) {
                    x::XContext::spin_send(&mut xsender, Err(res))
                } else {
                    Ok(())
                }
            });

            let mut ddc_pool = futures_cpupool::Builder::new()
                .pool_size(1)
                .name_prefix("DDC")
                .create();

            let mut uinput = uinput::Builder::new();
            uinput.name("screenstub");
            uinput.id(&InputId {
                bustype: input::sys::BUS_VIRTUAL,
                vendor: 0x16c0,
                product: 0x05df,
                version: 1,
            });
            uinput.x_config();
            let uinput = uinput.create()?;
            let uinput_path = uinput.path().to_owned();
            info!("uinput path: {}", uinput_path.display());

            let mut core = Core::new()?;
            let core_handle = core.handle();

            let timer = Timer::default();

            let mut qemu = Rc::new(RefCell::new(Qemu::new(config.qemu, core_handle.clone())));

            let mut user = UserProcess::new(core_handle.clone(),
                ddc_pool,
                convert_display(config.monitor),
                convert_input(config.host_source),
                convert_input(config.guest_source),
                config.ddc,
                qemu.clone(),
            );
            let user = Rc::new(RefCell::new(user));

            let mut events = event::Events::new();
            config.hotkeys.into_iter()
                .map(convert_hotkey)
                .for_each(|(hotkey, on_press)| events.add_hotkey(hotkey, on_press));
            config.key_remap.into_iter().for_each(|(from, to)| events.add_remap(from, to));

            let events = Rc::new(RefCell::new(events));

            let evdev_sleep = timer.sleep(Duration::from_secs(2)).map_err(Error::from);
            core_handle.spawn(qemu.borrow_mut().remove_evdev("screenstub-abs")
                .then(|_| evdev_sleep)
                .and_then({
                    let qemu = qemu.clone();
                    move |_| {
                        let mut qemu = qemu.borrow_mut(); // wtf rust?
                        qemu.add_evdev("screenstub-abs", uinput_path)
                    }
                }).map_err(|e| error!("Failed to add uinput device to qemu {} {:?}", e, e))
                .or_else(|_| Ok::<_, ()>(()))
            );

            let (inputsender, inputreceiver) = un_mpsc::channel(0x10);
            let uinput = uinput.to_sink(&core_handle)?;
            core_handle.spawn(inputreceiver
                .map({
                    let events = events.clone();
                    move |e| events.borrow_mut().map_input_event(e)
                })
                .map_err(|_| -> Error { unreachable!() })
                .forward(uinput).map(drop).map_err(drop) // TODO: error handling
            );

            let (usersender, userreceiver) = un_mpsc::channel::<Rc<ConfigEvent>>(0x08);
            core_handle.spawn(userreceiver
                .map_err(|_| -> Error { unreachable!() })
                .map({
                    let user = user.clone();
                    move |userevent| stream::iter_ok::<_, Error>(user.borrow_mut().process_user_event(&userevent))
                })
                .flatten()
                .map(|e| match e {
                    ProcessedUserEvent::UserEvent(e) => (Some(
                        e.or_else(|e| {
                            warn!("UserEvent failed {} {:?}", e, e);
                            Ok(())
                        })
                    ), None),
                    ProcessedUserEvent::XRequest(e) => (None, Some(e)),
                }).unzip_spawn(&core_handle, |s| s.filter_map(|e| e)
                    .map_err(|_| -> mpsc::SendError<_> { unreachable!() }) // ugh come on
                    .forward(xreqsender).map(drop).map_err(drop)
                ).map_err(|e| format_err!("{:?}", e))? // ugh can this even fail?
                .filter_map(|e| e)
                .buffer_unordered(8)
                .map_err(drop)
                .for_each(|_| Ok(()))
            );

            let (input_organic_sender, input_organic_receiver) = un_mpsc::channel(0x10);
            core_handle.spawn(input_organic_receiver
                .map_err(|_| -> Error { unreachable!() })
                .map({
                    let events = events.clone();
                    move |inputevent| stream::iter_ok::<_, Error>(events.borrow_mut().process_input_event(&inputevent))
                }).flatten()
                .map_err(|_| -> un_mpsc::SendError<_> { unreachable!() })
                .forward(usersender.clone()).map(drop).map_err(drop)
            );

            core.run(xreceiver
                .map_err(|_| -> Error { unreachable!() })
                .and_then(|e| e)
                .map({
                    let events = events.clone();
                    move |xevent| stream::iter_ok::<_, Error>(events.borrow_mut().process_x_event(&xevent))
                }).flatten()
                .map(|e| match e {
                    ProcessedXEvent::InputEvent(e) => (Some(e), None),
                    ProcessedXEvent::UserEvent(e) => (None, Some(e)),
                }).unzip_spawn(&core_handle, |s| s.filter_map(|e| e)
                    .map(convert_user_event)
                    .map_err(|_| -> un_mpsc::SendError<_> { unreachable!() }) // ugh come on
                    .forward(usersender).map(drop).map_err(drop)
                ).map_err(|e| format_err!("{:?}", e))? // ugh can this even fail?
                .filter_map(|e| e)
                .forward(inputsender.fanout(input_organic_sender)).map(drop).map_err(drop)
            ).unwrap();

            if let Err(e) = core.run(qemu.borrow_mut().remove_evdev("screenstub-abs")) {
                error!("Failed to remove uinput device from qemu {} {:?}", e, e);
            }

            if let Err(e) = core.run(
                stream::iter_result(
                    config.exit_events.into_iter()
                    .map(|e| user.borrow_mut().process_user_event(&e))
                    .flat_map(|e| e)
                    .map(|e| match e {
                        ProcessedUserEvent::UserEvent(e) => Ok(e),
                        ProcessedUserEvent::XRequest(x) => Err(format_err!("event {:?} cannot be processed at exit", x)),
                    })
                ).for_each(|_| Ok(()))
            ) {
                error!("Failed to run exit events: {} {:?}", e, e);
            }

            // TODO: go back to host at this point before exiting

            xthread.join().unwrap()?; // TODO: get this properly

            Ok(0)
        },
        _ => unreachable!("unknown command"),
    }
}

fn convert_user_event(event: UserEvent) -> Rc<ConfigEvent> {
    Rc::new(match event {
        UserEvent::ShowGuest => ConfigEvent::ShowGuest,
        UserEvent::ShowHost => ConfigEvent::ShowHost,
        UserEvent::UnstickGuest => ConfigEvent::UnstickGuest,
        UserEvent::UnstickHost => ConfigEvent::UnstickHost,
    })
}

fn convert_hotkey(hotkey: config::ConfigHotkey) -> (Hotkey<ConfigEvent>, bool) {
    (
        Hotkey::new(hotkey.triggers, hotkey.modifiers, hotkey.events),
        !hotkey.on_release,
    )
}

fn convert_display(monitor: config::ConfigMonitor) -> SearchDisplay {
    SearchDisplay {
        manufacturer_id: monitor.manufacturer,
        model_name: monitor.model,
        serial_number: monitor.serial,
        path: None, // TODO: i2c bus selection
    }
}

fn convert_input(input: config::ConfigInput) -> SearchInput {
    SearchInput {
        value: input.value,
        name: input.name,
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

pub struct UserProcess {
    grabs: HashMap<ConfigGrabMode, bool>,
    handle: Handle,
    ddc_pool: CpuPool,
    showing_guest: Rc<Cell<bool>>,
    input_guest: Arc<SearchInput>,
    input_host: Arc<SearchInput>,
    input_host_value: Arc<AtomicUsize>,
    ddc_host: ConfigDdcHost,
    ddc_guest: ConfigDdcGuest,
    #[cfg(feature = "with-ddcutil")]
    ddc: Arc<Mutex<Monitor>>,
    qemu: Rc<RefCell<Qemu>>,
}

impl UserProcess {
    fn new(handle: Handle, ddc_pool: CpuPool, display: SearchDisplay, input_host: SearchInput, input_guest: SearchInput, ddc: ConfigDdc, qemu: Rc<RefCell<Qemu>>) -> Self {
        UserProcess {
            grabs: Default::default(),
            handle: handle,
            showing_guest: Rc::new(Cell::new(false)),
            input_guest: Arc::new(input_guest),
            input_host: Arc::new(input_host),
            input_host_value: Arc::new(AtomicUsize::new(0x100)),
            ddc_host: ddc.host,
            ddc_guest: ddc.guest,
            #[cfg(feature = "with-ddcutil")]
            ddc: Arc::new(Mutex::new(Monitor::new(display))),
            ddc_pool: ddc_pool,
            qemu: qemu,
        }
    }

    fn grab(&mut self, grab: &ConfigGrab) -> Vec<ProcessedUserEvent> {
        let mode = grab.mode();

        let res = vec![match *grab {
            ConfigGrab::XCore => xreq(XRequest::Grab),
            ref grab => future::err(format_err!("grab unimplemented")).into(),
        }];

        *self.grabs.entry(mode).or_insert(true) = true;

        res
    }

    fn ungrab(&mut self, grab: ConfigGrabMode) -> Vec<ProcessedUserEvent> {
        let res = vec![match grab {
            ConfigGrabMode::XCore => xreq(XRequest::Ungrab),
            grab => future::err(format_err!("ungrab {:?} unimplemented", grab)).into(),
        }];

        *self.grabs.entry(grab).or_insert(false) = false;

        res
    }

    fn map_input_arg<S: AsRef<OsStr>>(&self, s: &S, input: Option<u8>) -> OsString {
        let s = s.as_ref();
        if let Some(input) = input {
            let bytes = s.as_bytes();
            if bytes == b"{}" {
                OsString::from(format!("{}", input))
            } else if bytes == b"{:x}" {
                OsString::from(format!("{:02x}", input))
            } else if bytes == b"0x{:x}" {
                OsString::from(format!("0x{:02x}", input))
            } else {
                s.to_owned()
            }
        } else {
            s.to_owned()
        }
    }

    fn show_guest(&mut self) -> Vec<ProcessedUserEvent> {
        let showing_guest = self.showing_guest.clone();
        match self.ddc_host {
            ConfigDdcHost::None => vec![future::ok(()).into()],
            #[cfg(feature = "with-ddcutil")]
            ConfigDdcHost::Libddcutil => {
                let ddc = self.ddc.clone();
                let input = self.input_guest.clone();
                let input_host = self.input_host.clone();
                let input_host_value = self.input_host_value.clone();
                vec![futures::sync::oneshot::spawn_fn(move || {
                    let mut ddc = ddc.lock().map_err(|e| format_err!("DDC mutex poisoned {:?}", e))?;
                    ddc.to_display()?;
                    if let Some(input) = ddc.our_input() {
                        if input_host.name.is_some() {
                            if let Some(input) = ddc.match_input(&input_host) {
                                input_host_value.store(input as _, Ordering::Relaxed);
                            }
                        } else {
                            input_host_value.store(input as _, Ordering::Relaxed);
                        }
                    }
                    if let Some(input) = ddc.match_input(&input) {
                        ddc.set_input(input)
                    } else {
                        Err(format_err!("DDC guest input source not found"))
                    }
                }, &self.ddc_pool)
                .inspect(move |&()| showing_guest.set(true))
                .into()]
            },
            ConfigDdcHost::Ddcutil => {
                vec![future::err(format_err!("ddcutil unimplemented")).into()]
            },
            ConfigDdcHost::Exec(ref args) => {
                let input = self.input_guest.value;
                vec![exec(&self.handle, args.into_iter().map(|i| self.map_input_arg(i, input)))
                    .inspect(move |&()| showing_guest.set(true))
                    .into()
                ]
            },
        }
    }

    fn show_host(&mut self) -> Box<Future<Item=(), Error=Error>> {
        let input_host_value = self.input_host_value.load(Ordering::Relaxed);
        let input = self.input_host.value.or_else(|| if input_host_value < 0x100 { Some(input_host_value as u8) } else { None });
        let showing_guest = self.showing_guest.clone();

        match self.ddc_guest {
            ConfigDdcGuest::None => {
                self.showing_guest.set(false);
                Box::new(future::ok(())) as Box<_>
            }, // TODO: not really sure why this is an option
            ConfigDdcGuest::Exec(ref args) => {
                let input = self.input_guest.value;
                Box::new(exec(&self.handle, args.into_iter().map(|i| self.map_input_arg(i, input)))
                    .inspect(move |&()| showing_guest.set(false))
                ) as Box<_>
            },
            ConfigDdcGuest::GuestExec(ref args) => {
                Box::new(
                    self.qemu.borrow_mut().guest_exec(args.into_iter().map(|i| self.map_input_arg(i, input)))
                    .inspect(move |&()| showing_guest.set(false))
                ) as Box<_>
            },
        }
    }

    fn process_user_event(&mut self, event: &ConfigEvent) -> Vec<ProcessedUserEvent> {
        trace!("process_user_event({:?})", event);
        info!("User event {:?}", event);
        match *event {
            ConfigEvent::ShowHost => {
                user(self.show_host())
            },
            ConfigEvent::ShowGuest => {
                self.show_guest()
            },
            ConfigEvent::Exec(ref args) => {
                vec![exec(&self.handle, args).into()]
            }
            ConfigEvent::ToggleGrab(ref grab) => {
                let mode = grab.mode();
                if self.grabs.get(&mode) == Some(&true) {
                    self.ungrab(mode)
                } else {
                    self.grab(grab)
                }
            }
            ConfigEvent::ToggleShow => {
                if self.showing_guest.get() {
                    user(self.show_host())
                } else {
                    self.show_guest()
                }
            },
            ConfigEvent::Grab(ref grab) => self.grab(grab),
            ConfigEvent::Ungrab(grab) => self.ungrab(grab),
            ConfigEvent::UnstickGuest => {
                vec![xreq(XRequest::UnstickGuest)] // TODO: this shouldn't be necessary as a xevent
            }
            ConfigEvent::UnstickHost => {
                vec![xreq(XRequest::UnstickHost)]
            }
            ConfigEvent::Poweroff => {
                user(self.qemu.borrow_mut().guest_poweroff())
            },
        }
    }
}

struct Qemu {
    comm: ConfigQemuComm,
    driver: ConfigQemuDriver,
    qmp: Option<String>,
    ga: Option<String>,
    handle: Handle,
}

impl Qemu {
    pub fn new(qemu: config::ConfigQemu, handle: Handle) -> Self {
        Qemu {
            comm: qemu.comm,
            driver: qemu.driver,
            qmp: qemu.qmp_socket,
            ga: qemu.ga_socket,
            handle: handle,
        }
    }

    // TODO: none of these need to be mut probably?
    pub fn guest_exec<I: IntoIterator<Item=S>, S: AsRef<OsStr>>(&mut self, args: I) -> Box<Future<Item=(), Error=Error>> {
        match self.comm {
            ConfigQemuComm::Qemucomm => {
                if let Some(ga) = self.ga.as_ref() {
                    exec(&self.handle,
                         ["qemucomm", "-g", &ga, "exec", "-w"]
                            .iter().map(|&s| s.to_owned())
                        .chain(args.into_iter().map(|s| s.as_ref().to_string_lossy().into_owned()))
                    )
                } else {
                    Box::new(future::err(format_err!("QEMU Guest Agent socket not provided"))) as Box<_>
                }
            },
            ConfigQemuComm::QMP => {
                unimplemented!()
            },
            ConfigQemuComm::Console => {
                unimplemented!()
            },
        }
    }

    pub fn add_evdev<I: AsRef<OsStr>, D: AsRef<OsStr>>(&mut self, id: I, device: D) -> Box<Future<Item=(), Error=Error>> {
        let id = id.as_ref().to_string_lossy();
        let device = format!("evdev={}", device.as_ref().to_string_lossy());

        let (driver, command) = match self.driver {
            ConfigQemuDriver::Virtio => ("virtio-input-host", "add_device"),
            ConfigQemuDriver::InputLinux => ("input-linux", "add_object"),
        };

        match self.comm {
            ConfigQemuComm::Qemucomm => {
                if let Some(qmp) = self.qmp.as_ref() {
                    exec(&self.handle, ["qemucomm", "-q", &qmp, command, driver, &id[..], &device].iter().cloned())
                } else {
                    Box::new(future::err(format_err!("QEMU QMP socket not provided"))) as Box<_>
                }
            },
            ConfigQemuComm::QMP => {
                unimplemented!()
            },
            ConfigQemuComm::Console => {
                unimplemented!()
            },
        }
    }

    pub fn remove_evdev<I: AsRef<OsStr>>(&mut self, id: I) -> Box<Future<Item=(), Error=Error>> {
        let id = id.as_ref().to_string_lossy();

        let command = match self.driver {
            ConfigQemuDriver::Virtio => "del_device",
            ConfigQemuDriver::InputLinux => "del_object",
        };

        match self.comm {
            ConfigQemuComm::Qemucomm => {
                if let Some(qmp) = self.qmp.as_ref() {
                    exec(&self.handle, ["qemucomm", "-q", &qmp, command, &id[..]].iter().cloned())
                } else {
                    Box::new(future::err(format_err!("QEMU QMP socket not provided"))) as Box<_>
                }
            },
            ConfigQemuComm::QMP => {
                unimplemented!()
            },
            ConfigQemuComm::Console => {
                unimplemented!()
            },
        }
    }

    pub fn guest_poweroff(&mut self) -> Box<Future<Item=(), Error=Error>> {
        match self.comm {
            ConfigQemuComm::Qemucomm => {
                if let Some(qmp) = self.qmp.as_ref() {
                    exec(&self.handle, ["qemucomm", "-q", &qmp, "poweroff"].iter().cloned())
                } else {
                    Box::new(future::err(format_err!("QEMU QMP socket not provided"))) as Box<_>
                }
            },
            ConfigQemuComm::QMP => {
                unimplemented!()
            },
            ConfigQemuComm::Console => {
                unimplemented!()
            },
        }
    }
}

fn exec<I: IntoIterator<Item=S>, S: AsRef<OsStr>>(ex: &Handle, args: I) -> Box<Future<Item=(), Error=Error>> {
    fn exit_status_error(status: ExitStatus) -> Result<(), Error> {
        if status.success() {
            Ok(())
        } else {
            Err(if let Some(code) = status.code() {
                format_err!("process exited with code {}", code)
            } else {
                format_err!("process exited with a failure")
            })
        }
    }

    let mut args = args.into_iter();
    if let Some(cmd) = args.next() {
        let child = Command::new(cmd).args(args).spawn_async(ex);
        Box::new(future::result(child)
            .and_then(|c| c).map_err(Error::from)
            .and_then(exit_status_error)
        ) as Box<_>
    } else {
        Box::new(future::err(format_err!("Missing exec command"))) as Box<_>
    }
}
