use futures::{Sink, Stream, SinkExt, StreamExt};
use anyhow::{Error, format_err};
use input_linux::{InputEvent, EventTime, KeyEvent, KeyState, Key, AbsoluteEvent, AbsoluteAxis, RelativeEvent, RelativeAxis, SynchronizeEvent};
use enumflags2::BitFlags;
use xproto::protocol::*;
use xproto::conversion::AsPrimitive;
use std::collections::BTreeMap;
use log::{trace, warn, info};

pub fn iter_bits(mut v: u32) -> impl Iterator<Item=usize> {
    let mut index = 0;
    std::iter::from_fn(move || {
        if v == 0 {
            None
        } else {
            let shift = v.trailing_zeros();
            v >>= shift + 1;
            let res = index + shift;
            index += shift + 1;
            Some(res as usize)
        }
    })
}

#[derive(Debug, Clone, Copy, Default)]
struct XState {
    pub width: u16,
    pub height: u16,
    pub running: bool,
    pub grabbed: bool,
}

#[derive(Debug)]
pub struct XInputEvent {
    time: u32,
    data: XInputEventData,
}

#[derive(Debug)]
pub enum XInputEventData {
    Mouse {
        x: i16,
        y: i16,
    },
    MouseRelative {
        axis: RelativeAxis,
        value: i32,
    },
    Button {
        pressed: bool,
        button: u8,
        state: BitFlags<xcore::KeyButMask>,
    },
    Key {
        pressed: bool,
        keycode: u8,
        keysym: Option<u32>,
        state: BitFlags<xcore::KeyButMask>,
    },
}

#[derive(Debug)]
pub enum XEvent {
    Visible(bool),
    Focus(bool),
    Close,
    Input(InputEvent),
}

#[derive(Debug)]
pub enum XRequest {
    Quit,
    UnstickHost,
    Grab {
        xcore: bool,
        confine: bool,
        motion: bool,
        devices: Vec<()>,
    },
    Ungrab,
}

type XConnection = xserver::stream::XConnection<xserver::stream::IoRead<'static>>;
type XSink = xserver::stream::XSink<xserver::stream::IoWrite<'static>>;

pub struct XContext {
    sink: XSink,
    window: xcore::Window,
    ext_input: xcore::QueryExtensionReply,
    ext_test: xcore::QueryExtensionReply,
    ext_dpms: Option<xcore::QueryExtensionReply>,
    ext_xkb: xcore::QueryExtensionReply,
    setup: xcore::Setup,

    keys: xcore::GetKeyboardMappingReply,
    mods: xcore::GetModifierMappingReply,
    devices: BTreeMap<xinput::DeviceId, xinput::XIDeviceInfo>,
    valuators: BTreeMap<(xinput::DeviceId, u16), xinput::DeviceClassDataValuator>,
    state: XState,
    display: xserver::Display,

    event_queue: Vec<XEvent>,

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
        let ext_input = sink.extension(ExtensionKind::Input).await.await?
            .expect("XInput required");
        let ext_xkb = sink.extension(ExtensionKind::Xkb).await.await?
            .expect("XKB required");
        let ext_test = sink.extension(ExtensionKind::Test).await.await?
            .expect("XTest required");
        let ext_dpms = sink.extension(ExtensionKind::DPMS).await.await?;
        let _ = sink.execute(xinput::XIQueryVersionRequest {
            major_opcode: ext_input.major_opcode,
            major_version: 2,
            minor_version: 3,
        }).await.await?;

        let _ = sink.execute(xkb::UseExtensionRequest {
            major_opcode: ext_xkb.major_opcode,
            wanted_major: 1,
            wanted_minor: 0,
        }).await.await?;

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
                    event_mask: (xcore::EventMask::VisibilityChange
                        | xcore::EventMask::KeyPress | xcore::EventMask::KeyRelease | xcore::EventMask::ButtonPress | xcore::EventMask::ButtonRelease | xcore::EventMask::PointerMotion | xcore::EventMask::ButtonMotion
                        | xcore::EventMask::PropertyChange
                        | xcore::EventMask::StructureNotify
                        | xcore::EventMask::FocusChange).into(),
                }),
                .. Default::default()
            },
        }).await.await?;

        sink.execute(xinput::XISelectEventsRequest {
            major_opcode: ext_input.major_opcode,
            window,
            masks: vec![
                xinput::EventMask {
                    deviceid: xinput::Device::All.into(),
                    mask: vec![xinput::XIEventMask::DeviceChanged.into()],
                },
            ],
        }).await.await?;

        sink.execute(xkb::PerClientFlagsRequest {
            // Inhibit KeyRelease events normally generated by autorepeat
            major_opcode: ext_xkb.major_opcode,
            device_spec: xkb::ID::UseCoreKbd.into(), // according to xlib XkbSetDetectableAutoRepeat?
            change: xkb::PerClientFlag::DetectableAutoRepeat.into(),
            value: xkb::PerClientFlag::DetectableAutoRepeat.into(),
            .. Default::default()
        }).await.await?;

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

            keys,
            mods,
            setup,
            state: Default::default(),
            devices: Default::default(),
            valuators: Default::default(),
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

    pub fn keycode(&self, code: u8) -> u8 {
        code - self.setup.min_keycode
    }

    pub fn keysym(&self, code: u8) -> Option<u32> {
        let modifier = 0; // TODO: ?
        match self.keys.keysyms.get(code as usize * self.keys.keysyms_per_keycode as usize + modifier).cloned() {
            Some(0) => None,
            keysym => keysym,
        }
    }

    pub fn stop(&mut self) {
        log::trace!("XContext::stop()");

        self.state.running = false;
    }

    pub async fn xmain<I: Stream<Item=XRequest>, O: Sink<XEvent>>(name: &str, instance: &str, class: &str, i: I, o: O) -> Result<(), Error> {
        let (conn, sink, display) = Self::connect().await?;
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

        let mut xcontext = Self::new(sink, display, setup).await?;

        let i = i.fuse();
        futures::pin_mut!(i);
        futures::pin_mut!(o);

        xcontext.state.running = true;
        xcontext.set_wm_name(name).await?;
        xcontext.set_wm_class(instance, class).await?;
        xcontext.map_window().await?;

        while xcontext.state.running {
            futures::select_biased! {
                req = i.next() => match req {
                    None => break,
                    Some(req) => xcontext.process_request(&req).await?,
                },
                e = event_receiver.next() => match e {
                    None => break,
                    Some(e) => xcontext.process_event(&e?).await?,
                },
            }

            while let Some(e) = xcontext.event_queue_pop() {
                let _ = o.send(e).await; // break if err?
            }
        }

        drop(event_receiver);
        join.await?;

        Ok(())
    }

    fn handle_grab_status(&self, status: xcore::GrabStatus) -> Result<(), Error> {
        if status == xcore::GrabStatus::Success as _ {
            Ok(())
        } else {
            Err(format_err!("X failed to grab with status {:?}", status))
        }
    }

    pub async fn process_request(&mut self, request: &XRequest) -> Result<(), Error> {
        trace!("processing X request {:?}", request);

        Ok(match *request {
            XRequest::Quit => {
                self.stop();
            },
            XRequest::UnstickHost => {
                let keys = self.sink.execute(xcore::QueryKeymapRequest { }).await.await?;
                let mut keycode = 0usize;
                for &key in &keys.keys {
                    for i in 0u32..8 {
                        if key & (1 << i) != 0 {
                            self.sink.execute(xtest::FakeInputRequest {
                                major_opcode: self.ext_test.major_opcode,
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

    async fn update_grab(&mut self, grab: bool) -> Result<(), Error> {
        self.sink.execute(xinput::XISelectEventsRequest {
            major_opcode: self.ext_input.major_opcode,
            window: self.screen().root,
            masks: vec![
                xinput::EventMask {
                    deviceid: xinput::Device::All.into(),
                    mask: vec![
                        if grab {
                            xinput::XIEventMask::RawMotion.into()
                        } else {
                            Default::default()
                        }
                    ],
                },
            ],
        }).await.await?;
        self.state.grabbed = grab;

        // XI SetDeviceMode?

        Ok(())
    }

    async fn update_key_mappings(&mut self) -> Result<(), Error> {
        let setup = &self.setup;
        self.keys = self.sink.execute(xcore::GetKeyboardMappingRequest {
            first_keycode: setup.min_keycode,
            count: setup.max_keycode - setup.min_keycode,
        }).await.await?;

        Ok(())
    }

    async fn update_mappings(&mut self) -> Result<(), Error> {
        self.update_key_mappings().await?;
        self.mods = self.sink.execute(xcore::GetModifierMappingRequest { }).await.await?;

        Ok(())
    }

    async fn update_valuators(&mut self) -> Result<(), Error> {
        self.devices = self.sink.execute(xinput::XIQueryDeviceRequest {
            major_opcode: self.ext_input.major_opcode,
            deviceid: xinput::Device::All.into(),
        }).await.await?.infos
            .into_iter().map(|info| (info.deviceid.value(), info)).collect();

        for (_, device) in &self.devices {
            self.valuators = device.classes.iter().filter_map(|class| match class.data {
                xinput::DeviceClassData::Valuator(val) => Some(((device.deviceid.value(), val.number), val)),
                _ => None,
            }).collect();
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
                self.state.width = event.width;
                self.state.height = event.height;
            },
            ExtensionEvent::Core(xcore::Events::FocusOut(..)) => {
                self.event_queue.push(XEvent::Focus(false));
            },
            ExtensionEvent::Core(xcore::Events::FocusIn(..)) => {
                self.event_queue.push(XEvent::Focus(true));
            },
            ExtensionEvent::Core(e @ xcore::Events::ButtonPress(..)) | ExtensionEvent::Core(e @ xcore::Events::ButtonRelease(..)) => {
                let (pressed, event) = match e {
                    xcore::Events::ButtonPress(event) => (true, event),
                    xcore::Events::ButtonRelease(event) => (false, &event.0),
                    _ => unsafe { core::hint::unreachable_unchecked() },
                };
                let event = XInputEvent {
                    time: event.time,
                    data: XInputEventData::Button {
                        pressed,
                        button: event.detail,
                        state: event.state.get(),
                    },
                };
                self.convert_x_events(&event)
            },
            ExtensionEvent::Core(xcore::Events::MotionNotify(event)) => {
                if self.state.grabbed {
                    // TODO: proper filtering
                    return Ok(())
                }
                let event = XInputEvent {
                    time: event.time,
                    data: XInputEventData::Mouse {
                        x: event.event_x,
                        y: event.event_y,
                    },
                };
                self.convert_x_events(&event)
            },
            ExtensionEvent::Core(e @ xcore::Events::KeyPress(..)) | ExtensionEvent::Core(e @ xcore::Events::KeyRelease(..)) => {
                let (pressed, event) = match e {
                    xcore::Events::KeyPress(event) => (true, event),
                    xcore::Events::KeyRelease(event) => (false, &event.0),
                    _ => unsafe { core::hint::unreachable_unchecked() },
                };

                let keycode = self.keycode(event.detail);
                let keysym = self.keysym(keycode);

                let event = XInputEvent {
                    time: event.time,
                    data: XInputEventData::Key {
                        pressed,
                        keycode,
                        keysym: if keysym == Some(0) { None } else { keysym },
                        state: event.state.into(),
                    },
                };
                self.convert_x_events(&event)
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
                    self.convert_x_events(&event)
                }
            },*/
            ExtensionEvent::Input(xinput::Events::RawMotion(event)) => {
                let event = &event.0;
                let axis_info = self.valuator_info(event.deviceid.value())
                    .ok_or_else(|| format_err!("XInput device unknown for event: {:?}", event))?;
                // TODO: could be relative or abs?
                // TODO: figure out which axis are scroll wheels via ScrollClass - there are multiple entries per valuator?
                let mut values = event.axisvalues.iter().zip(&event.axisvalues_raw);
                for &valuator_mask in &event.valuator_mask {
                    for axis in iter_bits(valuator_mask)/*.zip(&mut values)*/ {
                        let (value, value_raw) = values.next().unwrap();
                        let valuator = match self.valuators.get(&(event.deviceid.value(), axis as u16)) {
                            Some(val) => val,
                            _ => continue,
                        };
                        let &xinput::Fp3232 { integral, frac } = value_raw;
                        let value = (integral as i64) << 32;
                        let value = if integral < 0 {
                            value - frac as i64
                        } else {
                            value + frac as i64
                        };
                        let event = XInputEvent {
                            time: event.time.value(),
                            data: match valuator.mode.get() {
                                xinput::ValuatorMode::Relative => XInputEventData::MouseRelative {
                                    axis: match axis {
                                        // TODO: match by label instead? Are these indexes fixed?
                                        0 => RelativeAxis::X,
                                        1 => RelativeAxis::Y,
                                        2 => RelativeAxis::Wheel,
                                        3 => RelativeAxis::HorizontalWheel,
                                        _ => continue,
                                    },
                                    value: (value >> 32) as i32,
                                },
                                xinput::ValuatorMode::Absolute => continue /*XInputEventData::Mouse {
                                    axis: match axis {
                                        0 => RelativeAxis::X,
                                        1 => RelativeAxis::Y,
                                    },
                                    value: (value >> 2) as i32,
                                }*/,
                            },
                        };
                        self.convert_x_events(&event)
                    }
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

    fn x_button(button: u8) -> Option<Key> {
        match button {
            1 => Some(Key::ButtonLeft),
            2 => Some(Key::ButtonMiddle),
            3 => Some(Key::ButtonRight),
            4 => Some(Key::ButtonGearUp), // or wheel y axis
            5 => Some(Key::ButtonWheel), // Key::ButtonGearDown, or wheel y axis
            // TODO: 6/7 is horizontal scroll left/right, but I think this requires sending relative wheel events?
            8 => Some(Key::ButtonSide),
            9 => Some(Key::ButtonExtra),
            10 => Some(Key::ButtonForward),
            11 => Some(Key::ButtonBack),
            _ => None,
        }
    }

    fn x_keycode(key: u8) -> Option<Key> {
        match Key::from_code(key as _) {
            Ok(code) => Some(code),
            Err(..) => None,
        }
    }

    fn x_keysym(_key: u32) -> Option<Key> {
        unimplemented!()
    }

    fn key_event(time: EventTime, key: Key, pressed: bool) -> InputEvent {
        KeyEvent::new(time, key, KeyState::pressed(pressed)).into()
    }

    /*fn event_time(millis: xcb::Time) -> EventTime {
        let seconds = millis / 1000;
        let remaining = seconds % 1000;
        let usecs = remaining as i64 * 1000;

        EventTime::new(seconds as i64, usecs)
    }*/

    fn convert_x_events(&mut self, e: &XInputEvent) {
        //let time = Self::event_time(e.time);
        let time = Default::default();
        match e.data {
            XInputEventData::Mouse { x, y } => {
                self.event_queue.extend([
                    (self.state.width, x, AbsoluteAxis::X),
                    (self.state.height, y, AbsoluteAxis::Y),
                ].iter()
                    .filter(|&&(dim, new, _)| dim != 0)
                    .map(|&(dim, new, axis)| (dim, (new.max(0) as u16).min(dim), axis))
                    .map(|(dim, new, axis)| AbsoluteEvent::new(
                        time,
                        axis,
                        0x7fff.min(new as i32 * 0x8000 / dim as i32),
                    )).map(|e| XEvent::Input(e.into())));
            },
            XInputEventData::MouseRelative { axis, value } => {
                self.event_queue.push(XEvent::Input(RelativeEvent::new(time, axis, value).into()));
            },
            XInputEventData::Button { pressed, button, state: _ } => {
                if let Some(button) = Self::x_button(button) {
                    self.event_queue.push(XEvent::Input(Self::key_event(time, button, pressed).into()));
                } else {
                    warn!("unknown X button {:?}", button);
                }
            },
            XInputEventData::Key { pressed, keycode, keysym, state: _ } => {
                if let Some(key) = Self::x_keycode(keycode) {
                    self.event_queue.push(XEvent::Input(Self::key_event(time, key, pressed).into()));
                } else {
                    warn!("unknown X keycode {} keysym {:?}", keycode, keysym);
                }
            },
        }
        self.event_queue.push(XEvent::Input(SynchronizeEvent::report(time).into()));
    }

    fn event_queue_pop(&mut self) -> Option<XEvent> {
        if self.event_queue.is_empty() {
            None
        } else {
            Some(self.event_queue.remove(0))
        }
    }
}

/*impl Stream for XContext {
    type Item = Result<XEvent, Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        {
            let this = self.as_mut().get_mut();

            if !this.state.running {
                return Poll::Ready(None)
            }

            match this.event_queue_pop() {
                Some(res) =>
                    return Poll::Ready(Some(Ok(res))),
                None => (),
            }

            // wait for x event
            // blocking version: this.pump()
            match this.poll()? {
                Some(event) =>
                    tokio::task::block_in_place(|| this.process_event(&event))?,
                None => {
                    match this.fd.poll_read_ready(cx) {
                        Poll::Pending => {
                            this.stop_waker = Some(cx.waker().clone());
                            return Poll::Pending
                        },
                        Poll::Ready(ready) => {
                            let mut ready = ready?;
                            if let Some(event) = this.conn.poll_for_event() {
                                // poll returned None, so we know next_event is empty
                                this.next_event = Some(event);
                                ready.retain_ready()
                            } else {
                                ready.clear_ready()
                            }
                        },
                    }
                },
            }
        }

        // recurse to return new events or wait on IO
        self.poll_next(cx)
    }
}

impl Sink<XRequest> for XContext {
    type Error = Error;

    fn start_send(self: Pin<&mut Self>, item: XRequest) -> Result<(), Self::Error> {
        let this = self.get_mut();

        this.next_request = Some(item);

        Ok(())
    }

    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let this = self.get_mut();

        if let Some(req) = this.next_request.take() {
            // TODO: consider storing errors instead of returning them here
            tokio::task::block_in_place(|| this.process_request(&req))?;
            Poll::Ready(Ok(()))
        } else {
            Poll::Ready(Ok(()))
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.poll_ready(cx)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        if self.state.running {
            ready!(self.as_mut().poll_flush(cx))?;
            self.stop();
        }
        Poll::Ready(Ok(()))
    }
}*/
