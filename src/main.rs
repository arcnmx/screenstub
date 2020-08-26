#![recursion_limit = "1024"]

extern crate input_linux as input;
extern crate screenstub_uinput as uinput;
extern crate screenstub_config as config;
extern crate screenstub_event as event;
extern crate screenstub_qemu as qemu;
extern crate screenstub_ddc as ddc;
extern crate screenstub_x as x;

use std::process::exit;
use std::time::Duration;
use std::pin::Pin;
use std::sync::Arc;
use std::io::{self, Write};
use futures::channel::{mpsc, oneshot};
use futures::{future, TryFutureExt, FutureExt, StreamExt, SinkExt};
use failure::{Error, format_err};
use log::{warn, error};
use clap::{Arg, App, SubCommand, AppSettings};
use input::{InputId, Key, RelativeAxis, AbsoluteAxis, InputEvent, EventKind};
use config::{Config, ConfigEvent, ConfigSourceName};
use event::{Hotkey, UserEvent, ProcessedXEvent};
use qemu::Qemu;
use route::Route;
use spawner::Spawner;
use sources::Sources;
use process::Process;
use ddc::{Monitor, DdcMonitor};
use x::XRequest;

mod route;
mod grab;
mod filter;
mod sources;
mod exec;
mod process;
mod util;
mod spawner;

type Events = event::Events<Arc<ConfigEvent>>;

const EVENT_BUFFER: usize = 8;

fn main() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let spawner = Arc::new(Spawner::new());

    let code = match runtime.block_on(main_result(&spawner)) {
        Ok(code) => code,
        Err(e) => {
            let _ = writeln!(io::stderr(), "{:?} {}", e, e);
            1
        },
    };

    runtime.block_on(spawner.join_timeout(Duration::from_secs(2))).unwrap();

    runtime.shutdown_timeout(Duration::from_secs(1));

    exit(code);
}

async fn main_result(spawner: &Arc<Spawner>) -> Result<i32, Error> {
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
        ).arg(Arg::with_name("screen")
            .short("s")
            .long("screen")
            .value_name("SCREEN")
            .takes_value(true)
            .help("Configuration screen index")
        ).subcommand(SubCommand::with_name("x")
            .about("Start the KVM with a fullscreen X window")
        ).subcommand(SubCommand::with_name("detect")
            .about("Detect available DDC/CI displays and their video inputs")
        ).subcommand(SubCommand::with_name("source")
            .about("Change the configured monitor input source")
            .arg(Arg::with_name("confirm")
                 .short("c")
                 .long("confirm")
                 .help("Check that the VM is running before switching input")
            ).arg(Arg::with_name("source")
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

        let f = File::open(config)?;
        serde_yaml::from_reader(f)?
    } else {
        Config::default()
    };

    let screen_index = matches.value_of("screen").map(|s| s.parse()).unwrap_or(Ok(0))?;
    let screen = config.screens.into_iter().nth(screen_index)
        .ok_or_else(|| format_err!("expected a screen config"))?;

    match matches.subcommand() {
        ("x", Some(..)) => {
            let (x_sender, mut x_receiver) = mpsc::channel(0x20);
            let (mut xreq_sender, xreq_receiver) = mpsc::channel(0x08);
            let xinstance = screen.x_instance.unwrap_or("auto".into());
            let xmain = tokio::spawn(async move {
                let x = x::XContext::xmain("screenstub", &xinstance, "screenstub", xreq_receiver, x_sender);
                if let Err(e) = x.await {
                    error!("X Error: {}: {:?}", e, e);
                }
            }).map_err(From::from);

            let (keyboard_driver, relative_driver, absolute_driver) =
                (config.qemu.keyboard_driver(), config.qemu.relative_driver(), config.qemu.absolute_driver());

            let mut events = event::Events::new();
            config.hotkeys.into_iter()
                .map(convert_hotkey)
                .for_each(|(hotkey, on_press)| events.add_hotkey(hotkey, on_press));
            config.key_remap.into_iter().for_each(|(from, to)| events.add_remap(from, to));

            let events = Arc::new(events);

            let qemu = Arc::new(Qemu::new(config.qemu.qmp_socket, config.qemu.ga_socket));

            let ddc = screen.ddc.unwrap_or_default();
            let mut sources = Sources::new(qemu.clone(), screen.monitor, screen.host_source, screen.guest_source, ddc.host, ddc.guest, ddc.minimal_delay);
            sources.fill().await?;

            let (mut event_sender, mut event_recv) = mpsc::channel(EVENT_BUFFER);
            let (error_sender, mut error_recv) = mpsc::channel(1);

            let process = Process::new(
                config.qemu.routing, keyboard_driver, relative_driver, absolute_driver, config.exit_events,
                qemu.clone(), events.clone(), sources, xreq_sender.clone(), event_sender.clone(), error_sender.clone(),
                spawner.clone(),
            );

            process.devices_init().await?;

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
            let mut events_keyboard = route_keyboard.spawn(spawner, error_sender.clone());

            let mut route_relative = Route::new(config.qemu.routing, qemu.clone(), "screenstub-route-mouse".into(), bus.clone(), repeat);
            if let Some(builder) = route_relative.builder() {
                builder
                    .name("screenstub-mouse")
                    .x_config_rel()
                    .id(&uinput_id);
            }
            let mut events_relative = route_relative.spawn(spawner, error_sender.clone());

            let mut route_absolute = Route::new(config.qemu.routing, qemu.clone(), "screenstub-route-tablet".into(), bus, repeat);
            if let Some(builder) = route_absolute.builder() {
                builder
                    .name("screenstub-tablet")
                    .x_config_abs()
                    .id(&uinput_id);
            }
            let mut events_absolute = route_absolute.spawn(spawner, error_sender.clone());

            let x_filter = process.x_filter();

            let process = Arc::new(process);

            let (mut user_sender, user_receiver) = mpsc::channel::<Arc<ConfigEvent>>(0x08);
            let mut user_receiver = user_receiver
                .map({
                    let process = process.clone();
                    move |event| process.process_user_event(&event)
                });

            let (event_loop, event_loop_abort) = future::abortable({
                let events = events.clone();
                let process = process.clone();
                let mut user_sender = user_sender.clone();
                async move {
                    while let Some(event) = event_recv.next().await {
                        let user_events = events.process_input_event(&event);
                        let inputevent = events.map_input_event(event);
                        let user_sender = &mut user_sender;
                        let f1 = async move {
                            for e in user_events {
                                let _ = user_sender.send(e.clone()).await;
                            }
                        };
                        let is_mouse = process.is_mouse();

                        let events_keyboard = &mut events_keyboard;
                        let events_relative = &mut events_relative;
                        let events_absolute = &mut events_absolute;
                        let f2 = async move {
                            match map_event_kind(&inputevent, is_mouse) {
                                EventKind::Key => {
                                    let _ = events_keyboard.send(inputevent).await;
                                },
                                EventKind::Relative => {
                                    let _ = events_relative.send(inputevent).await;
                                },
                                EventKind::Absolute => {
                                    let _ = events_absolute.send(inputevent).await;
                                },
                                EventKind::Synchronize => {
                                    let _ = future::try_join3(
                                        events_keyboard.send(inputevent),
                                        events_relative.send(inputevent),
                                        events_absolute.send(inputevent)
                                    ).await;
                                },
                                _ => (),
                            }
                        };
                        let _ = future::join(f1, f2).await;
                    }
                }
            });
            let event_loop = tokio::spawn(event_loop.map(drop))
                .map_err(Error::from);

            let (xevent_exit_send, xevent_exit_recv) = oneshot::channel();
            let mut xevent_exit_recv = xevent_exit_recv.fuse();
            let xevent_loop = tokio::spawn({
                async move {
                    while let Some(xevent) = x_receiver.next().await {
                        for e in events.process_x_event(&xevent) {
                            match e {
                                ProcessedXEvent::UserEvent(e) => {
                                    let _ = user_sender.send(convert_user_event(e)).await;
                                },
                                ProcessedXEvent::InputEvent(e) if x_filter.filter_event(&e) => {
                                    let _ = event_sender.send(e).await;
                                },
                                ProcessedXEvent::InputEvent(_) => (),
                            }
                        }
                    }

                    let _ = xevent_exit_send.send(());
                }
            }).map_err(From::from);

            let res = loop {
                futures::select! {
                    _ = xevent_exit_recv => break Ok(()),
                    error = error_recv.next() => if let Some(error) = error {
                        break Err(error)
                    },
                    event = user_receiver.next() => if let Some(event) = event {
                        tokio::spawn(async move {
                            match Pin::from(event).await {
                                Err(e) =>
                                    warn!("User event failed {} {:?}", e, e),
                                Ok(()) => (),
                            }
                        });
                    },
                }
            };

            let _ = xreq_sender.send(XRequest::Quit).await; // ensure we kill x
            drop(xreq_sender);
            drop(process);

            // seal off senders
            event_loop_abort.abort();
            future::try_join3(
                event_loop,
                xevent_loop,
                xmain,
            ).await?;

            res.map(|()| 0)
        },
        ("detect", Some(..)) => {
            Monitor::enumerate()?.into_iter().try_for_each(|mut m| {
                let sources = m.sources()?;
                let current_source = m.get_source()?;
                println!("{}", m);
                sources.into_iter().for_each(|i|
                    println!("  Source: {} = 0x{:02x}{}",
                        ConfigSourceName::from_value(i).map(|i| i.to_string()).unwrap_or("Unknown".into()),
                        i,
                        if i == current_source { " (Active)" } else { "" }
                    )
                );

                Ok::<_, Error>(())
            })?;

            Ok(0)
        },
        ("source", Some(matches)) => {
            let ddc = screen.ddc.unwrap_or_default();

            let qemu = Arc::new(Qemu::new(config.qemu.qmp_socket, config.qemu.ga_socket));
            let sources = Sources::new(qemu, screen.monitor, screen.host_source, screen.guest_source, ddc.host, ddc.guest, ddc.minimal_delay);

            match matches.value_of("source") {
                Some("host") => sources.show(true, true).await,
                Some("guest") => sources.show(false, true).await,
                _ => unreachable!("unknown source to switch to"),
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

fn convert_user_event(event: UserEvent) -> Arc<ConfigEvent> {
    Arc::new(match event {
        UserEvent::Quit => ConfigEvent::Exit,
        UserEvent::ShowGuest => ConfigEvent::ShowGuest,
        UserEvent::ShowHost => ConfigEvent::ShowHost,
        UserEvent::UnstickGuest => ConfigEvent::UnstickGuest,
        UserEvent::UnstickHost => ConfigEvent::UnstickHost,
    })
}

fn convert_hotkey(hotkey: config::ConfigHotkey) -> (Hotkey<Arc<ConfigEvent>>, bool) {
    (
        Hotkey::new(hotkey.triggers, hotkey.modifiers, hotkey.events.into_iter().map(Arc::new)),
        !hotkey.on_release,
    )
}

fn map_event_kind(inputevent: &InputEvent, is_mouse: bool) -> EventKind {
    match inputevent.kind {
        EventKind::Key if Key::from_code(inputevent.code).map(|k| k.is_button()).unwrap_or(false) =>
            if is_mouse {
                EventKind::Relative
            } else {
                EventKind::Absolute
            },
        EventKind::Key =>
            EventKind::Key,
        EventKind::Absolute if inputevent.code == AbsoluteAxis::Volume as u16 =>
            EventKind::Key, // is this right?
        EventKind::Relative if RelativeAxis::from_code(inputevent.code).map(|a| axis_is_relative(a)).unwrap_or(false) =>
            EventKind::Relative,
        EventKind::Absolute if AbsoluteAxis::from_code(inputevent.code).map(|a| axis_is_absolute(a)).unwrap_or(false) =>
            EventKind::Absolute,
        EventKind::Relative | EventKind::Absolute =>
            if is_mouse {
                EventKind::Relative
            } else {
                EventKind::Absolute
            },
        EventKind::Synchronize =>
            EventKind::Synchronize,
        kind => {
            warn!("unforwarded event {:?}", kind);
            kind
        },
    }
}
