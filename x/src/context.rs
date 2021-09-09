use config::ConfigInputEvent;
use enumflags2::BitFlags;
use futures::{Sink, Stream, SinkExt, StreamExt, FutureExt};
use anyhow::{Error, format_err};
use xproto::protocol::*;
use xproto::protocol::xcore::EventMask;
use xproto::conversion::AsPrimitive;
use std::collections::BTreeMap;
use log::{trace, warn, info};
use screenstub_config as config;
use crate::{Rect, XEvent, XRequest};
use crate::events::XEventQueue;
use crate::xinput::{XInput, XInputVersion};

pub async fn xmain<I: Stream<Item=XRequest>, O: Sink<XEvent>>(name: &str, instance: &str, class: &str, rect: Rect, i: I, o: O) -> Result<(), Error> {
    let (conn, sink, display) = XContext::connect().await?;
    let setup = conn.setup().clone();
    let mut conn = conn.fuse();

    let (mut event_sender, mut event_receiver) = futures::channel::mpsc::unbounded();

    let join = tokio::spawn(async move {
        while let Some(e) = conn.next().await {
            if event_sender.send(e).await.is_err() {
                break
            }
        }
    });

    let mut xcontext = XContext::new(sink, display, setup).await?;

    let i = i.fuse();
    futures::pin_mut!(i);
    futures::pin_mut!(o);

    xcontext.event_queue.set_bounds(rect);

    xcontext.state.running = true;
    xcontext.set_wm_name(name).await?;
    xcontext.set_wm_class(instance, class).await?;
    xcontext.map_window().await?;

    let mut idle = futures::future::Fuse::terminated();
    while xcontext.state.running {
        futures::select_biased! {
            req = i.next() => match req {
                None => break,
                Some(req) => xcontext.process_request(&req).await?,
            },
            e = event_receiver.next() => match e {
                None => break,
                Some(e) => {
                    idle = futures::future::ready(()).fuse();
                    xcontext.process_event(&e?).await?
                },
            },
            () = idle =>
                xcontext.event_queue.commit(),
        }

        while let Some(e) = xcontext.event_queue.pop() {
            let _ = o.send(e).await; // break if err?
        }
    }

    drop(event_receiver);
    join.await?;

    Ok(())
}

#[derive(Debug, Clone, Copy, Default)]
struct XState {
    pub running: bool,
    pub grabbed: bool,
}

pub type XConnection = xserver::stream::XConnection<xserver::stream::IoRead<'static>>;
pub type XSink = xserver::stream::XSink<xserver::stream::IoWrite<'static>>;

pub struct XContext {
    sink: XSink,
    window: xcore::Window,
    ext_input: Option<(xcore::QueryExtensionReply, XInputVersion)>,
    ext_test: Option<xcore::QueryExtensionReply>,
    ext_dpms: Option<xcore::QueryExtensionReply>,
    ext_xkb: Option<xcore::QueryExtensionReply>,
    setup: xcore::Setup,

    devices: BTreeMap<xinput::DeviceId, xinput::XIDeviceInfo>,
    grab_devices: Vec<config::ConfigInputDevice>,
    grab_raw: bool,
    xinput: XInput,
    state: XState,
    display: xserver::Display,

    config_mask_core: BitFlags<ConfigInputEvent>,
    config_mask_xi: BitFlags<ConfigInputEvent>,

    event_queue: XEventQueue,

    atom_wm_state: xcore::Atom,
    atom_wm_protocols: xcore::Atom,
    atom_wm_delete_window: xcore::Atom,
    atom_net_wm_state: xcore::Atom,
    atom_net_wm_state_fullscreen: xcore::Atom,
}

//unsafe impl Send for XContext { }

impl XContext {
    pub async fn connect() -> Result<(XConnection, XSink, xserver::Display), Error> {
        let display = xserver::Display::new(None)?;
        let ((r, w), auth) = xserver::stream::open_display(&display).await?;
        let (conn, sink) = xserver::stream::XConnection::connect(auth, r, w).await?;

        Ok((conn, sink, display))
    }

    async fn new(mut sink: XSink, display: xserver::Display, setup: xcore::Setup) -> Result<Self, Error> {
        let screen = setup.roots.get(display.screen as usize).unwrap();
        let window = sink.generate_id().await?;
        let ext_input = sink.extension(ExtensionKind::Input).await.await?;
        let ext_xkb = sink.extension(ExtensionKind::Xkb).await.await?;
        let ext_test = sink.extension(ExtensionKind::Test).await.await?;
        let ext_dpms = sink.extension(ExtensionKind::DPMS).await.await?;
        let ext_input = if let Some(xinput) = ext_input {
            XInputVersion::query(&mut sink, &xinput).await?
                .map(|v| (xinput, v))
        } else { None };

        if let Some(xkb) = &ext_xkb {
            let _ = sink.execute(xkb::UseExtensionRequest {
                major_opcode: xkb.major_opcode,
                wanted_major: 1,
                wanted_minor: 0,
            }).await.await?;
        }

        let config_mask_xi = match &ext_input {
            Some((xinput, version)) if version >= &XInputVersion::_2_0 =>
                BitFlags::all(),
            xinput => {
                if xinput.is_some() {
                    warn!("XInput 1.x unsupported");
                }
                Default::default()
            },
        };
        let config_mask_core = BitFlags::all() & !config_mask_xi;

        sink.execute(xcore::CreateWindowRequest {
            depth: xcore::WindowClass::CopyFromParent.into(),
            wid: window,
            parent: screen.root,
            x: 0, y: 0,
            width: screen.width_in_pixels, height: screen.height_in_pixels,
            border_width: 0,
            class: xcore::WindowClass::InputOutput.into(),
            visual: screen.root_visual,
            value_list: xcore::CreateWindowRequestValueList {
                back_pixel: Some(xcore::CreateWindowRequestValueListBackPixel {
                    background_pixel: screen.black_pixel,
                }),
                event_mask: Some(xcore::CreateWindowRequestValueListEventMask {
                    event_mask: (Self::core_events() | Self::core_input_events(config_mask_core)).into(),
                }),
                .. Default::default()
            },
        }).await.await?;

        let grab_raw = false;
        if let Some((xinput, version)) = &ext_input {
            if version >= &XInputVersion::_2_0 {
                sink.execute(xinput::XISelectEventsRequest {
                    major_opcode: xinput.major_opcode,
                    window,
                    masks: vec![
                        xinput::EventMask {
                            deviceid: xinput::Device::All.into(),
                            mask: vec![xinput::XIEventMask::DeviceChanged.into()],
                        },
                        xinput::EventMask {
                            deviceid: xinput::Device::AllMaster.into(),
                            mask: version.events_mask(grab_raw, config_mask_xi),
                        },
                    ],
                }).await.await?;
            } else {
                unimplemented!("XInput 1.x")
            }
        }

        if let Some(xkb) = &ext_xkb {
            sink.execute(xkb::PerClientFlagsRequest {
                // Inhibit KeyRelease events normally generated by autorepeat
                major_opcode: xkb.major_opcode,
                device_spec: xkb::ID::UseCoreKbd.into(), // according to xlib XkbSetDetectableAutoRepeat?
                change: xkb::PerClientFlag::DetectableAutoRepeat.into(),
                value: xkb::PerClientFlag::DetectableAutoRepeat.into(),
                .. Default::default()
            }).await.await?;
        }

        let (keys, mods) = (
            sink.execute(xcore::GetKeyboardMappingRequest {
                first_keycode: setup.min_keycode,
                count: setup.max_keycode - setup.min_keycode,
            }).await.await?,
            sink.execute(xcore::GetModifierMappingRequest { }).await.await?,
        );

        Ok(Self {
            atom_wm_state: sink.intern_atom("WM_STATE").await.await?,
            atom_wm_protocols: sink.intern_atom("WM_PROTOCOLS").await.await?,
            atom_wm_delete_window: sink.intern_atom("WM_DELETE_WINDOW").await.await?,
            atom_net_wm_state: sink.intern_atom("_NET_WM_STATE").await.await?,
            atom_net_wm_state_fullscreen: sink.intern_atom("_NET_WM_STATE_FULLSCREEN").await.await?,

            xinput: XInput::new(setup.min_keycode, keys, mods),
            setup,
            state: Default::default(),
            devices: Default::default(),
            grab_devices: Default::default(),
            grab_raw,
            config_mask_core,
            config_mask_xi,
            event_queue: Default::default(),
            ext_input,
            ext_test,
            ext_xkb,
            ext_dpms,
            display,

            sink,
            window,
        })
    }

    pub fn screen(&self) -> &xcore::Screen {
        self.setup.roots.get(self.display.screen as usize).unwrap()
    }

    pub async fn set_wm_name(&mut self, name: &str) -> Result<(), Error> {
        // TODO: set _NET_WM_NAME instead? or both?
        self.sink.execute(xcore::ChangePropertyRequest {
            mode: xcore::PropMode::Replace,
            window: self.window,
            property: xcore::AtomEnum::WM_NAME.into(),
            type_: xcore::AtomEnum::STRING.into(),
            data: xcore::ChangePropertyRequestData::Data8(xcore::ChangePropertyRequestDataData8 {
                data: name.into(),
            }),
        }).await.await.map(drop).map_err(From::from)
    }

    pub async fn set_wm_class(&mut self, instance: &str, class: &str) -> Result<(), Error> {
        // TODO: ensure neither class or instance contain nul byte
        let wm_class_string = format!("{}\0{}", instance, class);

        self.sink.execute(xcore::ChangePropertyRequest {
            mode: xcore::PropMode::Replace,
            window: self.window,
            property: xcore::AtomEnum::WM_CLASS.into(),
            type_: xcore::AtomEnum::STRING.into(),
            data: xcore::ChangePropertyRequestData::Data8(xcore::ChangePropertyRequestDataData8 {
                data: wm_class_string.into(),
            }),
        }).await.await.map(drop).map_err(From::from)
    }

    pub async fn map_window(&mut self) -> Result<(), Error> {
        self.sink.execute(xcore::ChangePropertyRequest {
            mode: xcore::PropMode::Replace,
            window: self.window,
            property: self.atom_wm_protocols.into(),
            type_: xcore::AtomEnum::ATOM.into(),
            data: xcore::ChangePropertyRequestData::Data32(xcore::ChangePropertyRequestDataData32 {
                data: vec![self.atom_wm_delete_window.into()],
            }),
        }).await.await?;

        self.sink.execute(xcore::ChangePropertyRequest {
            mode: xcore::PropMode::Append,
            window: self.window,
            property: self.atom_net_wm_state.into(),
            type_: xcore::AtomEnum::ATOM.into(),
            data: xcore::ChangePropertyRequestData::Data32(xcore::ChangePropertyRequestDataData32 {
                data: vec![self.atom_net_wm_state_fullscreen.into()],
            }),
        }).await.await?;

        self.update_valuators().await?;

        self.sink.execute(xcore::MapWindowRequest {
            window: self.window,
        }).await.await?;

        /*xcb::grab_button(&self.conn,
            false, // owner_events?
            self.window,
            xcb::BUTTON_MASK_ANY as _,
            xcb::GRAB_MODE_ASYNC as _,
            xcb::GRAB_MODE_ASYNC as _,
            self.window,
            xcb::NONE,
            xcb::BUTTON_INDEX_ANY as _,
            xcb::MOD_MASK_ANY as _,
        ).request_check()?;*/

        Ok(())
    }

    pub fn stop(&mut self) {
        log::trace!("XContext::stop()");

        self.state.running = false;
    }

    fn handle_grab_status(&self, status: xcore::GrabStatus) -> Result<(), Error> {
        if status == xcore::GrabStatus::Success as _ {
            Ok(())
        } else {
            Err(format_err!("X failed to grab with status {:?}", status))
        }
    }

    fn core_events() -> BitFlags<EventMask> {
        EventMask::VisibilityChange
            | EventMask::PropertyChange
            | EventMask::StructureNotify
            | EventMask::FocusChange
    }

    fn core_input_events(config_mask: BitFlags<ConfigInputEvent>) -> BitFlags<EventMask> {
        let mut res = BitFlags::default();
        if config_mask.contains(ConfigInputEvent::Key) {
            res |= EventMask::KeyPress | EventMask::KeyRelease;
        }
        if config_mask.contains(ConfigInputEvent::Button) {
            res |= EventMask::ButtonPress | EventMask::ButtonRelease;
        }
        if config_mask.contains(ConfigInputEvent::Relative) {
            res |= EventMask::PointerMotion | EventMask::ButtonMotion;
        }
        res
    }

    /*fn event_mask(&self) -> BitFlags<EventMask> {
        Self::core_events() | blah
    }*/

    pub async fn process_request(&mut self, request: &XRequest) -> Result<(), Error> {
        trace!("processing X request {:?}", request);

        Ok(match *request {
            XRequest::Quit => {
                self.stop();
            },
            XRequest::UnstickHost => {
                if let Some(xtest) = &self.ext_test {
                    let keys = self.sink.execute(xcore::QueryKeymapRequest { }).await.await?;
                    let mut keycode = 0usize;
                    for &key in &keys.keys {
                        for i in 0u32..8 {
                            if key & (1 << i) != 0 {
                                self.sink.execute(xtest::FakeInputRequest {
                                    major_opcode: xtest.major_opcode,
                                    type_: xcore::KeyReleaseEvent::NUMBER as _,
                                    detail: keycode as _,
                                    time: xcore::Time::CurrentTime.into(),
                                    root: xcore::WindowEnum::None.into(),
                                    root_x: 0,
                                    root_y: 0,
                                    deviceid: 0, // apparently xcb::NONE, but 0 is Device::AllMaster or something?
                                }).await.await?;
                            }
                            keycode += 1;
                        }
                    }
                }
            },
            XRequest::Grab { xcore, motion, confine, ref devices } => {
                if xcore {
                    let status = self.sink.execute(xcore::GrabKeyboardRequest {
                        owner_events: false, // I don't quite understand how this works
                        grab_window: self.window,
                        time: xcore::Time::CurrentTime.into(),
                        pointer_mode: xcore::GrabMode::Async.into(),
                        keyboard_mode: xcore::GrabMode::Async.into(),
                    }).await.await?;
                    self.handle_grab_status(status.status)?;

                    let status = self.sink.execute(xcore::GrabPointerRequest {
                        owner_events: false, // I don't quite understand how this works
                        grab_window: self.window,
                        event_mask: (xcore::EventMask::ButtonPress | xcore::EventMask::ButtonRelease | xcore::EventMask::PointerMotion | xcore::EventMask::ButtonMotion).as_(),
                        pointer_mode: xcore::GrabMode::Async.into(),
                        keyboard_mode: xcore::GrabMode::Async.into(),
                        confine_to: if confine {
                            self.window.into()
                        } else {
                            xcore::WindowEnum::None.into()
                        },
                        cursor: xcore::CursorEnum::None.into(),
                        time: xcore::Time::CurrentTime.into(),
                    }).await.await?;
                    self.handle_grab_status(status.status)?;
                }
                self.grab_devices = if devices.is_empty() {
                    vec![
                        Default::default()
                    ]
                } else {
                    devices.clone()
                };
                if motion {
                    self.update_grab(true).await?;
                }
            },
            XRequest::Ungrab => {
                self.sink.execute(xcore::UngrabKeyboardRequest {
                    time: xcore::Time::CurrentTime.into(),
                }).await.await?;
                self.sink.execute(xcore::UngrabPointerRequest {
                    time: xcore::Time::CurrentTime.into(),
                }).await.await?;
                self.update_grab(false).await?;
            },
        })
    }

    fn grab_masks<'a>(&'a self, grab: bool) -> impl Iterator<Item=xinput::EventMask> + 'a {
        fn device_matches(dev: &xinput::XIDeviceInfo, spec: &config::ConfigInputDevice) -> bool {
            if !spec.name.is_empty() && spec.name.as_bytes() != &dev.name[..] {
                return false
            }

            if let Some(kind) = spec.kind {
                let kind = match kind {
                    config::ConfigInputDeviceKind::Keyboard => if spec.master {
                        xinput::DeviceType::MasterKeyboard
                    } else {
                        xinput::DeviceType::SlaveKeyboard
                    },
                    config::ConfigInputDeviceKind::Mouse | config::ConfigInputDeviceKind::Tablet => if spec.master {
                        xinput::DeviceType::MasterPointer
                    } else {
                        xinput::DeviceType::SlavePointer
                    },
                };

                if kind != dev.type_.get() {
                    return false
                }
            }

            true
        }

        let devices = &self.devices;
        self.grab_devices.iter().filter_map(move |spec| {
            let deviceid = match spec.id {
                Some(id) => Some(xproto::enums::AltEnum::with_value(id)),
                None if spec.is_any() => Some(if spec.master {
                    xinput::Device::AllMaster
                } else {
                    xinput::Device::All
                }.into()),
                None => devices.values().find(|dev| device_matches(dev, spec)).map(|dev| dev.deviceid),
            };

            deviceid.map(|deviceid| xinput::EventMask {
                deviceid,
                mask: if grab {
                    vec![
                        xinput::XIEventMask::RawMotion.into()
                    ]
                } else {
                    vec![
                        Default::default()
                    ]
                }
            })
        })
    }

    async fn update_grab(&mut self, grab: bool) -> Result<(), Error> {
        if let Some((xinput, version)) = &self.ext_input {
            if version >= &XInputVersion::_2_0 {
                self.sink.execute(xinput::XISelectEventsRequest {
                    major_opcode: xinput.major_opcode,
                    window: self.screen().root,
                    masks: self.grab_masks(grab).collect(),
                }).await.await?;
            } else {
                unimplemented!("XInput 1.x")
            }
        }
        self.state.grabbed = grab;

        // XI SetDeviceMode?

        Ok(())
    }

    async fn update_key_mappings(&mut self) -> Result<(), Error> {
        let setup = &self.setup;
        self.xinput.keys = self.sink.execute(xcore::GetKeyboardMappingRequest {
            first_keycode: setup.min_keycode,
            count: setup.max_keycode - setup.min_keycode,
        }).await.await?;

        Ok(())
    }

    async fn update_mappings(&mut self) -> Result<(), Error> {
        self.update_key_mappings().await?;
        self.xinput.mods = self.sink.execute(xcore::GetModifierMappingRequest { }).await.await?;

        Ok(())
    }

    async fn update_valuators(&mut self) -> Result<(), Error> {
        self.devices = if let Some((xinput, version)) = &self.ext_input {
            if version >= &XInputVersion::_2_0 {
                self.sink.execute(xinput::XIQueryDeviceRequest {
                    major_opcode: xinput.major_opcode,
                    deviceid: xinput::Device::All.into(),
                }).await.await?.infos
                    .into_iter().map(|info| (info.deviceid.value(), info)).collect()
            } else {
                unimplemented!("XInput 1.x")
            }
        } else {
            Default::default()
        };

        self.xinput.valuators.clear();
        for (_, device) in &self.devices {
            self.xinput.valuators.extend(device.classes.iter().filter_map(|class| match class.data {
                xinput::DeviceClassData::Valuator(val) => Some(((device.deviceid.value(), val.number), val)),
                _ => None,
            }));
        }

        if self.state.grabbed {
            self.update_grab(true).await?;
        }

        Ok(())
    }

    fn valuator_info(&self, deviceid: xinput::DeviceId) -> Option<&xinput::XIDeviceInfo> {
        self.devices.get(&deviceid)
    }

    pub async fn process_event(&mut self, event: &ExtensionEvent) -> Result<(), Error> {
        trace!("processing X event {:?}", event);

        Ok(match event {
            ExtensionEvent::Core(xcore::Events::VisibilityNotify(event)) => {
                let dpms_blank = if let Some(ext_dpms) = &self.ext_dpms {
                    let info = self.sink.execute(dpms::InfoRequest {
                        major_opcode: ext_dpms.major_opcode,
                    }).await.await?;

                    info.power_level.get() != dpms::DPMSMode::On
                } else {
                    false
                };
                self.event_queue.push(if dpms_blank {
                    XEvent::Visible(false)
                } else {
                    match event.state {
                        xcore::Visibility::FullyObscured =>
                            XEvent::Visible(false),
                        xcore::Visibility::Unobscured =>
                            XEvent::Visible(true),
                        xcore::Visibility::PartiallyObscured =>
                            XEvent::Visible(true), // TODO: ??
                    }
                });
            },
            ExtensionEvent::Core(xcore::Events::ClientMessage(event)) => {
                let atom = match event.data {
                    xcore::ClientMessageEventData::Data32(d) => d.data[0],
                    _ => unimplemented!(),
                };
                if atom == self.atom_wm_delete_window.xid() {
                    self.event_queue.push(XEvent::Close);
                } else {
                    let atom = self.sink.execute(xcore::GetAtomNameRequest {
                        atom: atom.into(),
                    }).await.await?;
                    info!("unknown X client message {:?}", atom.name);
                }
            },
            ExtensionEvent::Core(xcore::Events::PropertyNotify(event)) => {
                match event.atom {
                    atom if atom == self.atom_wm_state => {
                        let r = self.sink.execute(xcore::GetPropertyRequest {
                            delete: false,
                            window: event.window,
                            property: atom,
                            type_: xcore::GetPropertyType::Any.into(),
                            long_offset: 0,
                            long_length: 0,
                        }).await.await?;
                        let x = match &r.value {
                            xcore::GetPropertyReplyValue::Data32(d) => d.data.get(0),
                            _ => None,
                        };
                        let window_state_withdrawn = 0;
                        // 1 is back but unobscured also works so ??
                        let window_state_iconic = 3;
                        match x {
                            Some(&state) if state == window_state_withdrawn || state == window_state_iconic => {
                                self.event_queue.push(XEvent::Visible(false));
                            },
                            Some(&state) => {
                                info!("unknown WM_STATE {}", state);
                            },
                            None => {
                                warn!("expected WM_STATE state value");
                            },
                        }
                    },
                    atom => {
                        let atom = self.sink.execute(xcore::GetAtomNameRequest {
                            atom: atom.into(),
                        }).await.await?;
                        info!("unknown property notify {:?}", atom.name);
                    },
                }
            },
            ExtensionEvent::Core(xcore::Events::MappingNotify(event)) => {
                self.update_mappings().await?;
            },
            ExtensionEvent::Core(xcore::Events::ConfigureNotify(event)) => {
                self.event_queue.set_size(event.width, event.height);
            },
            ExtensionEvent::Core(xcore::Events::FocusOut(..)) => {
                self.event_queue.push(XEvent::Focus(false));
            },
            ExtensionEvent::Core(xcore::Events::FocusIn(..)) => {
                self.event_queue.push(XEvent::Focus(true));
            },
            ExtensionEvent::Core(xcore::Events::ButtonPress(event)) => {
                self.event_queue.push_x_event(&self.xinput.process_event_buttonpress(event))
            },
            ExtensionEvent::Core(xcore::Events::ButtonRelease(event)) => {
                self.event_queue.push_x_event(&self.xinput.process_event_buttonrelease(event))
            },
            ExtensionEvent::Core(xcore::Events::MotionNotify(event)) => {
                if self.state.grabbed {
                    // TODO: proper filtering
                    return Ok(())
                }
                self.event_queue.push_x_event(&self.xinput.process_event_motion(event))
            },
            ExtensionEvent::Core(xcore::Events::KeyPress(event)) => {
                self.event_queue.push_x_event(&self.xinput.process_event_keypress(event))
            },
            ExtensionEvent::Core(xcore::Events::KeyRelease(event)) => {
                self.event_queue.push_x_event(&self.xinput.process_event_keyrelease(event))
            },
            ExtensionEvent::Input(xinput::Events::KeyPress(event)) => {
                if !event.flags.get().contains(xinput::KeyEventFlags::KeyRepeat) {
                    self.event_queue.push_x_event(&self.xinput.process_event_xi2_keypress(event))
                }
            },
            ExtensionEvent::Input(xinput::Events::KeyRelease(event)) => {
                self.event_queue.push_x_event(&self.xinput.process_event_xi2_keyrelease(event))
            },
            ExtensionEvent::Input(xinput::Events::ButtonPress(event)) => {
                self.event_queue.push_x_event(&self.xinput.process_event_xi2_buttonpress(event))
            },
            ExtensionEvent::Input(xinput::Events::ButtonRelease(event)) => {
                self.event_queue.push_x_event(&self.xinput.process_event_xi2_buttonrelease(event))
            },
            /*ExtensionEvent::Input(e @ xinput::Events::KeyPress(..)) | ExtensionEvent::Input(e @ xinput::Events::KeyRelease(..)) => {
                let (pressed, event) = match e {
                    xinput::Events::KeyPress(event) => (true, event),
                    xinput::Events::KeyRelease(event) => (false, &event.0),
                    _ => unsafe { core::hint::unreachable_unchecked() },
                };

                if !event.flags.get().contains(xinput::KeyEventFlags::KeyRepeat) {
                    let keycode = self.keycode(event.detail as _);
                    let keysym = self.keysym(keycode);

                    let event = XInputEvent {
                        time: event.time.value(),
                        data: XInputEventData::Key {
                            pressed,
                            keycode,
                            keysym: if keysym == Some(0) { None } else { keysym },
                            state: event.state.into(),
                        },
                    };
                    self.event_queue.push_x_event(&event)
                }
            },*/
            ExtensionEvent::Input(xinput::Events::Motion(event)) => {
                for event in self.xinput.process_event_xi2_motion(event, self.config_mask_xi) {
                    self.event_queue.push_x_event(&event);
                }
            },
            ExtensionEvent::Input(xinput::Events::RawMotion(event)) => {
                for event in self.xinput.process_event_xi2_motion_raw(event, self.config_mask_xi) {
                    self.event_queue.push_x_event(&event);
                }
            },
            ExtensionEvent::Input(xinput::Events::DevicePresenceNotify(event)) => {
                self.update_valuators().await?;
            },
            event => {
                info!("unknown X event {:?}", event);
            },
        })
    }
}
