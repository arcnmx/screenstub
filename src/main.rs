#[macro_use]
extern crate log;
extern crate env_logger;
extern crate futures;
extern crate futures_cpupool;
extern crate futures_stream_select_all;
#[macro_use]
extern crate failure;
extern crate input_linux as input;
extern crate screenstub_uinput as uinput;
extern crate screenstub_config as config;
extern crate screenstub_event as event;
extern crate screenstub_qemu as qemu;
extern crate screenstub_ddc as ddc;
extern crate screenstub_x as x;
extern crate tokio_unzip;
extern crate tokio_timer;
extern crate tokio_fuse;
extern crate tokio_core;
extern crate tokio_process;
extern crate serde_yaml;
extern crate tokio_qapi as qapi;
extern crate result;
extern crate clap;

use std::process::exit;
use std::thread::spawn;
use std::cell::RefCell;
use std::time::Duration;
use std::rc::Rc;
use std::io::{self, Write};
use tokio_core::reactor::Core;
use tokio_unzip::StreamUnzipExt;
use tokio_timer::Timer;
use futures::sync::mpsc;
use futures::unsync::mpsc as un_mpsc;
use futures::{Future, Stream, Sink, stream, future};
use futures::future::Either;
use failure::Error;
use clap::{Arg, App, SubCommand, AppSettings};
use input::{InputId, Key, RelativeAxis, AbsoluteAxis, EventKind};
use config::{Config, ConfigEvent};
use event::{Hotkey, UserEvent, ProcessedXEvent};
use qemu::Qemu;
use route::Route;
use inputs::Inputs;
use process::{Process, ProcessedUserEvent};
#[cfg(feature = "with-ddcutil")]
use ddc::Monitor;
use x::XRequest;

mod route;
mod grab;
mod filter;
mod inputs;
mod exec;
mod process;

const EVENT_BUFFER: usize = 8;

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
        ).subcommand(SubCommand::with_name("detect")
            .about("Detect available DDC/CI displays and their video inputs")
        ).subcommand(SubCommand::with_name("input")
            .about("Change the configured monitor input")
            .arg(Arg::with_name("confirm")
                 .short("c")
                 .long("confirm")
                 .help("Check that the VM is running before switching input")
            ).arg(Arg::with_name("input")
                .value_name("DEST")
                .takes_value(true)
                .required(true)
                .possible_values(&["host", "guest"])
                .help("Switch to either the host or guest monitor input")
            )
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
            let mut config = config.get(0).ok_or_else(|| format_err!("expected a screen config"))?.clone();

            let (mut x_sender, x_receiver) = mpsc::channel(0x20);
            let (xreq_sender, xreq_receiver) = mpsc::channel(0x08);
            let xthread = spawn(move || {
                if let Err(res) = x::XContext::xmain(xreq_receiver, &mut x_sender) {
                    x::XContext::spin_send(&mut x_sender, Err(res))
                } else {
                    Ok(())
                }
            });

            let mut core = Core::new()?;

            let qemu = Rc::new(Qemu::new(config.qemu.qmp_socket, config.qemu.ga_socket));

            let inputs = Rc::new(Inputs::new(core.handle(), qemu.clone(), config.monitor, config.host_source, config.guest_source, config.ddc.host, config.ddc.guest));

            let (event_sender, event_recv) = un_mpsc::channel(EVENT_BUFFER);
            let (error_sender, error_recv) = un_mpsc::channel(1);

            if let Some(driver) = config.qemu.driver {
                config.qemu.keyboard_driver = driver.clone();
                config.qemu.relative_driver = driver.clone();
                config.qemu.absolute_driver = driver;
            }
            let process = Process::new(core.handle(),
                config.qemu.routing, config.qemu.keyboard_driver, config.qemu.relative_driver, config.qemu.absolute_driver,
                qemu.clone(), inputs, event_sender.clone(), error_sender.clone(),
            );

            core.run(process.devices_init())?;
            core.run(process.set_is_mouse(false))?; // TODO: config option to start up in relative mode instead

            let uinput_id = InputId {
                bustype: input::sys::BUS_VIRTUAL,
                vendor: 0x16c0,
                product: 0x05df,
                version: 1,
            };

            let repeat = false;
            let bus = None;
            let mut route_keyboard = Route::new(config.qemu.routing, qemu.clone(), "screenstub-route-kbd".into(), bus.clone(), repeat);
            if let Some(builder) = route_keyboard.builder() {
                builder
                    .name("screenstub-kbd")
                    .x_config_key(repeat)
                    .id(&uinput_id);
            }
            let events_keyboard = route_keyboard.spawn(&core.handle(), error_sender.clone());

            let mut route_relative = Route::new(config.qemu.routing, qemu.clone(), "screenstub-route-mouse".into(), bus.clone(), repeat);
            if let Some(builder) = route_relative.builder() {
                builder
                    .name("screenstub-mouse")
                    .x_config_rel()
                    .id(&uinput_id);
            }
            let events_relative = route_relative.spawn(&core.handle(), error_sender.clone());

            let mut route_absolute = Route::new(config.qemu.routing, qemu.clone(), "screenstub-route-tablet".into(), bus, repeat);
            if let Some(builder) = route_absolute.builder() {
                builder
                    .name("screenstub-tablet")
                    .x_config_abs()
                    .id(&uinput_id);
            }
            let events_absolute = route_absolute.spawn(&core.handle(), error_sender.clone());

            let x_filter = process.x_filter();

            let process = Rc::new(process);

            core.handle().spawn(error_recv
                .inspect(|e| error!("{} {:?}", e, e))
                .into_future().map(|(e, _)| e)
                .then({ // TODO: what about recv errors??
                    let xreq = xreq_sender.clone();
                    move |e| if let Ok(Some(..)) = e {
                        future::Either::A(xreq.send(XRequest::Quit).map(drop))
                    } else {
                        future::Either::B(future::ok(()))
                    }
                }).or_else(|e| Ok(error!("failed to bail out: {} {:?}", e, e)))
            );

            let (user_sender, user_receiver) = un_mpsc::channel::<Rc<ConfigEvent>>(0x08);
            core.handle().spawn(user_receiver
                .map_err(|_| -> Error { unreachable!() })
                .map({
                    let process = process.clone();
                    move |userevent| stream::iter_ok::<_, Error>(process.process_user_event(&userevent))
                }).flatten()
                .map(|e| match e {
                    // TODO: give process a xreq_sender and forget the enum here
                    ProcessedUserEvent::UserEvent(e) => (Some(
                        e.or_else(|e| {
                            warn!("UserEvent failed {} {:?}", e, e);
                            Ok(())
                        })
                    ), None),
                    ProcessedUserEvent::XRequest(e) => (None, Some(e)),
                }).unzip_spawn(&core.handle(), |s| s.filter_map(|e| e)
                    .map_err(|_| -> mpsc::SendError<_> { unreachable!() }) // ugh come on
                    .forward(xreq_sender).map(drop).map_err(drop)
                ).map_err(|e| format_err!("{:?}", e))? // ugh can this even fail?
                .filter_map(|e| e)
                .buffer_unordered(8)
                .map_err(drop)
                .for_each(|_| Ok(()))
            );

            let mut events = event::Events::new();
            config.hotkeys.into_iter()
                .map(convert_hotkey)
                .for_each(|(hotkey, on_press)| events.add_hotkey(hotkey, on_press));
            config.key_remap.into_iter().for_each(|(from, to)| events.add_remap(from, to));

            let events = Rc::new(RefCell::new(events));

            let (event_process_send, event_process_recv) = un_mpsc::channel(EVENT_BUFFER);
            core.handle().spawn(event_process_recv
                .map_err(|_| -> Error { unreachable!() })
                .map({
                    let events = events.clone();
                    move |inputevent| stream::iter_ok::<_, Error>(events.borrow_mut().process_input_event(&inputevent))
                }).flatten()
                .map_err(|_| -> un_mpsc::SendError<_> { unreachable!() })
                .forward(user_sender.clone()).map(drop).map_err(drop)
            );

            let (event_route_send, event_route_recv) = un_mpsc::channel(EVENT_BUFFER);
            core.handle().spawn(
                event_recv.map_err(drop).forward(event_route_send.fanout(event_process_send).sink_map_err(drop))
                .map(drop).map_err(drop)
            );
            core.handle().spawn(event_route_recv
                .map_err(|_| -> Error { unreachable!() })
                .map({
                    let events = events.clone();
                    move |e| events.borrow_mut().map_input_event(e)
                })
                .fold((events_keyboard, events_relative, events_absolute), {
                    let process = process.clone();
                    move |(events_keyboard, events_relative, events_absolute), e| {
                        macro_rules! send_event {
                            ($e:ident => keyboard) => {
                                Either::A(Either::A(events_keyboard.send($e)
                                    .map(|events_keyboard| (events_keyboard, events_relative, events_absolute))
                                    .map_err(Error::from)))
                            };
                            ($e:ident => relative) => {
                                Either::A(Either::B(events_relative.send($e)
                                    .map(|events_relative| (events_keyboard, events_relative, events_absolute))
                                    .map_err(Error::from)))
                            };
                            ($e:ident => absolute) => {
                                Either::B(Either::A(events_absolute.send($e)
                                    .map(|events_absolute| (events_keyboard, events_relative, events_absolute))
                                    .map_err(Error::from)))
                            };
                            ($e:ident => all) => {
                                Either::B(Either::B(Either::A(
                                    events_keyboard.send($e)
                                        .and_then(move |events_keyboard|
                                            events_relative.send($e)
                                                .and_then(move |events_relative|
                                                    events_absolute.send($e)
                                                    .map(|events_absolute| (events_keyboard, events_relative, events_absolute))
                                                )
                                        )
                                    .map_err(Error::from)
                                )))
                            };
                            (pass) => {
                                Either::B(Either::B(Either::B(
                                    future::ok((events_keyboard, events_relative, events_absolute))
                                )))
                            };
                        }

                        let kind = match e.kind {
                            EventKind::Key if Key::from_code(e.code).map(|k| k.is_button()).unwrap_or(false) => {
                                if process.is_mouse() {
                                    EventKind::Relative
                                } else {
                                    EventKind::Absolute
                                }
                            },
                            EventKind::Key => {
                                EventKind::Key
                            },
                            EventKind::Absolute if e.code == AbsoluteAxis::Volume as u16 => {
                                EventKind::Key // is this right?
                            },
                            EventKind::Relative if RelativeAxis::from_code(e.code).map(|a| axis_is_relative(a)).unwrap_or(false) => {
                                EventKind::Relative
                            },
                            EventKind::Absolute if AbsoluteAxis::from_code(e.code).map(|a| axis_is_absolute(a)).unwrap_or(false) => {
                                EventKind::Absolute
                            },
                            EventKind::Relative | EventKind::Absolute => {
                                if process.is_mouse() {
                                    EventKind::Relative
                                } else {
                                    EventKind::Absolute
                                }
                            },
                            EventKind::Synchronize => {
                                EventKind::Synchronize
                            },
                            kind => {
                                warn!("unforwarded event {:?}", kind);
                                kind
                            },
                        };

                        match kind {
                            EventKind::Key => send_event!(e => keyboard),
                            EventKind::Relative => send_event!(e => relative),
                            EventKind::Absolute => send_event!(e => absolute),
                            EventKind::Synchronize => send_event!(e => all),
                            _ => send_event!(pass),
                        }
                    }
                }).map(drop)
                .or_else(|e| error_sender.send(e).map(drop).map_err(drop))
            );

            let handle = core.handle();
            core.run(x_receiver
                .map_err(|_| -> Error { unreachable!() })
                .and_then(|e| e)
                .map({
                    move |xevent| stream::iter_ok::<_, Error>(events.borrow_mut().process_x_event(&xevent))
                }).flatten()
                .map(|e| match e {
                    ProcessedXEvent::InputEvent(e) => (Some(e), None),
                    ProcessedXEvent::UserEvent(e) => (None, Some(e)),
                }).unzip_spawn(&handle, |s| s.filter_map(|e| e)
                    .map(convert_user_event)
                    .map_err(|_| -> un_mpsc::SendError<_> { unreachable!() }) // ugh come on
                    .forward(user_sender).map(drop).map_err(drop)
                ).map_err(|e| format_err!("{:?}", e))? // ugh can this even fail?
                .filter_map(|e| e)
                .filter(|e| x_filter.borrow().filter_event(e))
                .forward(event_sender).map(drop).map_err(drop)
            ).unwrap();

            if let Err(e) = core.run(
                stream::iter_result(
                    config.exit_events.into_iter()
                    .map(|e| process.process_user_event(&e))
                    .flat_map(|e| e)
                    .map(|e| match e {
                        ProcessedUserEvent::UserEvent(e) => Ok(e),
                        ProcessedUserEvent::XRequest(x) => Err(format_err!("event {:?} cannot be processed at exit", x)),
                    })
                ).and_then(|e| e).for_each(|()| Ok(()))
            ) {
                error!("Failed to run exit events: {} {:?}", e, e);
            }

            // TODO: go back to host at this point before exiting

            xthread.join().unwrap()?; // TODO: get this properly

            Ok(0)
        },
        #[cfg(feature = "with-ddcutil")]
        ("detect", Some(..)) => {
            Monitor::enumerate()?.into_iter().for_each(|m| {
                let info = m.info().unwrap();
                let inputs = m.inputs().unwrap();
                let current_input = m.our_input().unwrap();
                println!("Manufacturer: {}\nModel: {}\nSerial: {}",
                    info.manufacturer_id(), info.model_name(), info.serial_number()
                );
                inputs.into_iter().for_each(|i|
                    println!("  Input: {} = 0x{:02x}{}", i.1, i.0,
                        if *i.0 == current_input { " (Current)" } else { "" }
                    )
                );
            });

            Ok(0)
        },
        #[cfg(feature = "with-ddcutil")]
        ("input", Some(matches)) => {
            let config = config.get(0).ok_or_else(|| format_err!("expected a screen config"))?.clone();

            let mut core = Core::new()?;
            let qemu = Rc::new(Qemu::new(config.qemu.qmp_socket, config.qemu.ga_socket));
            let inputs = Inputs::new(core.handle(), qemu, config.monitor, config.host_source, config.guest_source, config.ddc.host, config.ddc.guest);

            match matches.value_of("input") {
                Some("host") => core.run(inputs.show_host()),
                Some("guest") => core.run(inputs.show_guest()), // TODO: bypass check for guest agent
                _ => unreachable!("unknown input to switch to"),
            }.map(|_| 0)
        },
        _ => unreachable!("unknown command"),
    }
}

fn axis_is_relative(axis: RelativeAxis) -> bool {
    match axis {
        RelativeAxis::X | RelativeAxis::Y => true,
        _ => false,
    }
}

fn axis_is_absolute(axis: AbsoluteAxis) -> bool {
    match axis {
        AbsoluteAxis::X | AbsoluteAxis::Y => true,
        _ => false,
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
