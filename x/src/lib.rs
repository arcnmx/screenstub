pub extern crate xcb;

use futures::{Sink, Stream, ready};
use anyhow::{Error, format_err};
use input_linux::{InputEvent, EventTime, KeyEvent, KeyState, Key, AbsoluteEvent, AbsoluteAxis, SynchronizeEvent};
use tokio::io::unix::AsyncFd;
use tokio::io::Interest;
use std::task::{Poll, Context, Waker};
use std::pin::Pin;
use log::{trace, warn, info};
use screenstub_fd::Fd;

#[derive(Debug, Clone, Copy, Default)]
struct XState {
    pub width: u16,
    pub height: u16,
    pub running: bool,
}

#[derive(Debug)]
pub struct XInputEvent {
    time: xcb::Time,
    data: XInputEventData,
}

#[derive(Debug)]
pub enum XInputEventData {
    Mouse {
        x: i16,
        y: i16,
    },
    Button {
        pressed: bool,
        button: xcb::Button,
        state: u16,
    },
    Key {
        pressed: bool,
        keycode: xcb::Keycode,
        keysym: Option<xcb::Keysym>,
        state: u16,
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
    Grab,
    Ungrab,
}

pub struct XContext {
    conn: xcb::Connection,
    fd: AsyncFd<Fd>,
    window: u32,

    keys: xcb::GetKeyboardMappingReply,
    mods: xcb::GetModifierMappingReply,
    state: XState,
    next_event: Option<xcb::GenericEvent>,
    next_request: Option<XRequest>,
    event_queue: Vec<XEvent>,
    stop_waker: Option<Waker>,

    atom_wm_state: xcb::Atom,
    atom_wm_protocols: xcb::Atom,
    atom_wm_delete_window: xcb::Atom,
    atom_net_wm_state: xcb::Atom,
    atom_net_wm_state_fullscreen: xcb::Atom,
}

unsafe impl Send for XContext { }

impl XContext {
    pub fn connect() -> Result<Self, Error> {
        let (conn, screen_num) = xcb::Connection::connect(None)?;
        let fd = {
            let fd = unsafe { xcb::ffi::base::xcb_get_file_descriptor(conn.get_raw_conn()) };
            AsyncFd::with_interest(fd.into(), Interest::READABLE)
        }?;
        let window = conn.generate_id();
        let (keys, mods) = {
            let setup = conn.get_setup();
            let screen = setup.roots().nth(screen_num as usize).unwrap();

            xcb::create_window(&conn,
                xcb::COPY_FROM_PARENT as _,
                window,
                screen.root(),
                0, 0,
                screen.width_in_pixels(), screen.height_in_pixels(),
                0,
                xcb::WINDOW_CLASS_INPUT_OUTPUT as _,
                screen.root_visual(),
                &[
                    (xcb::CW_BACK_PIXEL, screen.black_pixel()),
                    (
                        xcb::CW_EVENT_MASK,
                        xcb::EVENT_MASK_VISIBILITY_CHANGE | xcb::EVENT_MASK_PROPERTY_CHANGE |
                        xcb::EVENT_MASK_KEY_PRESS | xcb::EVENT_MASK_KEY_RELEASE |
                        xcb::EVENT_MASK_BUTTON_PRESS | xcb::EVENT_MASK_BUTTON_RELEASE |
                        xcb::EVENT_MASK_POINTER_MOTION | xcb::EVENT_MASK_BUTTON_MOTION |
                        xcb::EVENT_MASK_STRUCTURE_NOTIFY | xcb::EVENT_MASK_FOCUS_CHANGE
                    ),
                ]
            );

            (
                xcb::get_keyboard_mapping(&conn, setup.min_keycode(), setup.max_keycode() - setup.min_keycode()).get_reply()?,
                xcb::get_modifier_mapping(&conn).get_reply()?,
            )
        };

        Ok(Self {
            atom_wm_state: xcb::intern_atom(&conn, true, "WM_STATE").get_reply()?.atom(),
            atom_wm_protocols: xcb::intern_atom(&conn, true, "WM_PROTOCOLS").get_reply()?.atom(),
            atom_wm_delete_window: xcb::intern_atom(&conn, true, "WM_DELETE_WINDOW").get_reply()?.atom(),
            atom_net_wm_state: xcb::intern_atom(&conn, true, "_NET_WM_STATE").get_reply()?.atom(),
            atom_net_wm_state_fullscreen: xcb::intern_atom(&conn, true, "_NET_WM_STATE_FULLSCREEN").get_reply()?.atom(),

            keys,
            mods,
            state: Default::default(),
            next_event: None,

            event_queue: Default::default(),
            next_request: None,
            stop_waker: None,

            conn,
            fd,
            window,
        })
    }

    pub fn set_wm_name(&self, name: &str) -> Result<(), Error> {
        // TODO: set _NET_WM_NAME instead? or both?

        xcb::change_property(&self.conn,
            xcb::PROP_MODE_REPLACE as _,
            self.window,
            xcb::ATOM_WM_NAME,
            xcb::ATOM_STRING, 8,
            name.as_bytes()
        ).request_check()?;

        Ok(())
    }

    pub fn set_wm_class(&self, instance: &str, class: &str) -> Result<(), Error> {
        // TODO: ensure neither class or instance contain nul byte
        let wm_class_string = format!("{}\0{}", instance, class);

        xcb::change_property(&self.conn,
            xcb::PROP_MODE_REPLACE as _,
            self.window,
            xcb::ATOM_WM_CLASS,
            xcb::ATOM_STRING, 8,
            wm_class_string.as_bytes()
        ).request_check()?;

        Ok(())
    }

    pub fn map_window(&self) -> Result<(), Error> {
        xcb::change_property(&self.conn,
            xcb::PROP_MODE_REPLACE as _,
            self.window,
            self.atom_wm_protocols,
            xcb::ATOM_ATOM, 32,
            &[self.atom_wm_delete_window]
        ).request_check()?;

        xcb::change_property(&self.conn,
            xcb::PROP_MODE_APPEND as _,
            self.window,
            self.atom_net_wm_state,
            xcb::ATOM_ATOM, 32,
            &[self.atom_net_wm_state_fullscreen]
        ).request_check()?;

        xcb::map_window(&self.conn, self.window);

        self.flush()?;

        xcb::grab_button(&self.conn,
            false, // owner_events?
            self.window,
            xcb::BUTTON_MASK_ANY as _,
            xcb::GRAB_MODE_ASYNC as _,
            xcb::GRAB_MODE_ASYNC as _,
            self.window,
            xcb::NONE,
            xcb::BUTTON_INDEX_ANY as _,
            xcb::MOD_MASK_ANY as _,
        ).request_check()?;

        Ok(())
    }

    pub fn flush(&self) -> Result<(), xcb::ConnError> {
        if self.conn.flush() {
            Ok(())
        } else {
            Err(self.connection_error().unwrap())
        }
    }

    pub fn connection_error(&self) -> Option<xcb::ConnError> {
        self.conn.has_error().err()
    }

    pub fn connection(&self) -> &xcb::Connection {
        &self.conn
    }

    pub fn pump(&mut self) -> Result<xcb::GenericEvent, Error> {
        match self.next_event.take() {
            Some(e) => Ok(e),
            None => {
                if let Some(event) = self.conn.wait_for_event() {
                    Ok(event)
                } else {
                    Err(self.connection_error().unwrap().into())
                }
            }
        }
    }

    pub fn poll(&mut self) -> Result<Option<xcb::GenericEvent>, Error> {
        match self.next_event.take() {
            Some(e) => Ok(Some(e)),
            None => {
                if let Some(event) = self.conn.poll_for_event() {
                    Ok(Some(event))
                } else {
                    self.connection_error().map(|e| Err(e.into())).transpose()
                }
            }
        }
    }

    pub fn peek(&mut self) -> Option<&xcb::GenericEvent> {
        if self.next_event.is_none() {
            if let Some(event) = self.conn.poll_for_event() {
                Some(self.next_event.get_or_insert(event))
            } else {
                None
            }
        } else {
            self.next_event.as_ref()
        }
    }

    pub fn keycode(&self, code: xcb::Keycode) -> xcb::Keycode {
        code - self.conn.get_setup().min_keycode()
    }

    pub fn keysym(&self, code: xcb::Keycode) -> Option<xcb::Keysym> {
        let modifier = 0; // TODO: ?
        match self.keys.keysyms().get(code as usize * self.keys.keysyms_per_keycode() as usize + modifier).cloned() {
            Some(0) => None,
            keysym => keysym,
        }
    }

    pub fn stop(&mut self) {
        log::trace!("XContext::stop()");

        self.state.running = false;
        if let Some(waker) = self.stop_waker.take() {
            waker.wake();
        }
    }

    pub fn xmain(name: &str, instance: &str, class: &str) -> Result<Self, Error> {
        let mut xcontext = Self::connect()?;
        xcontext.state.running = true;
        xcontext.set_wm_name(name)?;
        xcontext.set_wm_class(instance, class)?;
        xcontext.map_window()?;
        Ok(xcontext)
    }

    fn handle_grab_status(&self, status: u8) -> Result<(), Error> {
        if status == xcb::GRAB_STATUS_SUCCESS as _ {
            Ok(())
        } else {
            Err(format_err!("X failed to grab with status code {}", status))
        }
    }

    pub fn process_request(&mut self, request: &XRequest) -> Result<(), Error> {
        trace!("processing X request {:?}", request);

        Ok(match *request {
            XRequest::Quit => {
                self.stop();
            },
            XRequest::UnstickHost => {
                let keys = xcb::query_keymap(&self.conn).get_reply()?;
                let keys = keys.keys();
                let mut keycode = 0usize;
                for &key in keys {
                    for i in 0..8 {
                        if key & (1 << i) != 0 {
                            xcb::test::fake_input(&self.conn,
                                xcb::KEY_RELEASE,
                                keycode as _,
                                xcb::CURRENT_TIME,
                                xcb::NONE, 0, 0,
                                xcb::NONE as _ // can't find documentation for this device_id argument?
                            ).request_check()?
                        }
                        keycode += 1;
                    }
                }
            },
            XRequest::Grab => {
                let status = xcb::grab_keyboard(&self.conn,
                    false, // owner_events, I don't quite understand how this works
                    self.window,
                    xcb::CURRENT_TIME,
                    xcb::GRAB_MODE_ASYNC as _,
                    xcb::GRAB_MODE_ASYNC as _,
                ).get_reply()?.status();
                self.handle_grab_status(status)?;
                let status = xcb::grab_pointer(&self.conn,
                    false, // owner_events, I don't quite understand how this works
                    self.window,
                    (xcb::EVENT_MASK_BUTTON_PRESS | xcb::EVENT_MASK_BUTTON_RELEASE | xcb::EVENT_MASK_POINTER_MOTION | xcb::EVENT_MASK_BUTTON_MOTION) as _,
                    xcb::GRAB_MODE_ASYNC as _,
                    xcb::GRAB_MODE_ASYNC as _,
                    self.window, // confine mouse to our window
                    xcb::NONE,
                    xcb::CURRENT_TIME,
                ).get_reply()?.status();
                self.handle_grab_status(status)?;
            },
            XRequest::Ungrab => {
                xcb::ungrab_keyboard(&self.conn, xcb::CURRENT_TIME).request_check()?;
                xcb::ungrab_pointer(&self.conn, xcb::CURRENT_TIME).request_check()?;
            },
        })
    }

    fn process_event(&mut self, event: &xcb::GenericEvent) -> Result<(), xcb::GenericError> {
        let kind = event.response_type() & !0x80;
        trace!("processing X event {}", kind);

        Ok(match kind {
            xcb::VISIBILITY_NOTIFY => {
                let event = unsafe { xcb::cast_event::<xcb::VisibilityNotifyEvent>(event) };

                let dpms_blank = {
                    let power_level = xcb::dpms::info(&self.conn).get_reply()
                        .map(|info| info.power_level() as u32);

                    power_level.unwrap_or(xcb::dpms::DPMS_MODE_ON) != xcb::dpms::DPMS_MODE_ON
                };
                self.event_queue.push(if dpms_blank {
                    XEvent::Visible(false)
                } else {
                    match event.state() as _ {
                        xcb::VISIBILITY_FULLY_OBSCURED => {
                            XEvent::Visible(false)
                        },
                        xcb::VISIBILITY_UNOBSCURED => {
                            XEvent::Visible(true)
                        },
                        state => {
                            warn!("unknown visibility {}", state);
                            return Ok(())
                        },
                    }
                });
            },
            xcb::CLIENT_MESSAGE => {
                let event = unsafe { xcb::cast_event::<xcb::ClientMessageEvent>(event) };

                match event.data().data32().get(0) {
                    Some(&atom) if atom == self.atom_wm_delete_window => {
                        self.event_queue.push(XEvent::Close);
                    },
                    Some(&atom) => {
                        let atom = xcb::get_atom_name(&self.conn, atom).get_reply();
                        info!("unknown X client message {:?}",
                            atom.as_ref().map(|a| a.name()).unwrap_or("UNKNOWN")
                        );
                    },
                    None => {
                        warn!("empty client message");
                    },
                }
            },
            xcb::PROPERTY_NOTIFY => {
                let event = unsafe { xcb::cast_event::<xcb::PropertyNotifyEvent>(event) };

                match event.atom() {
                    atom if atom == self.atom_wm_state => {
                        let r = xcb::get_property(&self.conn, false, event.window(), event.atom(), 0, 0, 1).get_reply()?;
                        let x = r.value::<u32>();
                        let window_state_withdrawn = 0;
                        // 1 is back but unobscured also works so ??
                        let window_state_iconic = 3;
                        match x.get(0) {
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
                        let atom = xcb::get_atom_name(&self.conn, atom).get_reply();
                        info!("unknown property notify {:?}",
                            atom.as_ref().map(|a| a.name()).unwrap_or("UNKNOWN")
                        );
                    },
                }
            },
            xcb::FOCUS_OUT | xcb::FOCUS_IN => {
                self.event_queue.push(XEvent::Focus(kind == xcb::FOCUS_IN));
            },
            xcb::KEY_PRESS | xcb::KEY_RELEASE => {
                let event = unsafe { xcb::cast_event::<xcb::KeyPressEvent>(event) };

                // filter out autorepeat events
                let peek = if let Some(peek) = self.peek() {
                    let peek_kind = peek.response_type() & !0x80;
                    match peek_kind {
                        xcb::KEY_PRESS | xcb::KEY_RELEASE => {
                            let peek_event = unsafe { xcb::cast_event::<xcb::KeyPressEvent>(peek) };
                            Some((peek_kind, peek_event.time(), peek_event.detail()))
                        },
                        _ => None,
                    }
                } else {
                    None
                };
                if let Some((peek_kind, peek_time, peek_detail)) = peek {
                    if peek_kind != kind && peek_time == event.time() && event.detail() == peek_detail {
                        // TODO: I think this only matters on release?
                        // repeat
                        return Ok(())
                    }
                }

                let keycode = self.keycode(event.detail());
                let keysym = self.keysym(keycode);

                let event = XInputEvent {
                    time: event.time(),
                    data: XInputEventData::Key {
                        pressed: kind == xcb::KEY_PRESS,
                        keycode,
                        keysym: if keysym == Some(0) { None } else { keysym },
                        state: event.state(),
                    },
                };
                self.convert_x_events(&event)
            },
            xcb::BUTTON_PRESS | xcb::BUTTON_RELEASE => {
                let event = unsafe { xcb::cast_event::<xcb::ButtonPressEvent>(event) };
                let event = XInputEvent {
                    time: event.time(),
                    data: XInputEventData::Button {
                        pressed: kind == xcb::BUTTON_PRESS,
                        button: event.detail(),
                        state: event.state(),
                    },
                };
                self.convert_x_events(&event)
            },
            xcb::MOTION_NOTIFY => {
                let event = unsafe { xcb::cast_event::<xcb::MotionNotifyEvent>(event) };
                let event = XInputEvent {
                    time: event.time(),
                    data: XInputEventData::Mouse {
                        x: event.event_x(),
                        y: event.event_y(),
                    },
                };
                self.convert_x_events(&event)
            },
            xcb::MAPPING_NOTIFY => {
                let setup = self.conn.get_setup();
                self.keys = xcb::get_keyboard_mapping(&self.conn, setup.min_keycode(), setup.max_keycode() - setup.min_keycode()).get_reply()?;
                self.mods = xcb::get_modifier_mapping(&self.conn).get_reply()?;
            },
            xcb::CONFIGURE_NOTIFY => {
                let event = unsafe { xcb::cast_event::<xcb::ConfigureNotifyEvent>(event) };
                self.state.width = event.width();
                self.state.height = event.height();
            },
            _ => {
                info!("unknown X event {}", event.response_type());
            },
        })
    }

    fn x_button(button: xcb::Button) -> Option<Key> {
        match button as _ {
            xcb::BUTTON_INDEX_1 => Some(Key::ButtonLeft),
            xcb::BUTTON_INDEX_2 => Some(Key::ButtonMiddle),
            xcb::BUTTON_INDEX_3 => Some(Key::ButtonRight),
            xcb::BUTTON_INDEX_4 => Some(Key::ButtonGearUp),
            xcb::BUTTON_INDEX_5 => Some(Key::ButtonWheel), // Key::ButtonGearDown
            // TODO: 6/7 is horizontal scroll left/right, but I think this requires sending relative wheel events?
            8 => Some(Key::ButtonSide),
            9 => Some(Key::ButtonExtra),
            // qemu input-linux.c doesn't support fwd/back, but virtio probably does
            10 => Some(Key::ButtonForward),
            11 => Some(Key::ButtonBack),
            _ => None,
        }
    }

    fn x_keycode(key: xcb::Keycode) -> Option<Key> {
        match Key::from_code(key as _) {
            Ok(code) => Some(code),
            Err(..) => None,
        }
    }

    fn x_keysym(_key: xcb::Keysym) -> Option<Key> {
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
            XInputEventData::Button { pressed, button, state: _ } => {
                if let Some(button) = Self::x_button(button) {
                    self.event_queue.push(XEvent::Input(Self::key_event(time, button, pressed).into()));
                } else {
                    warn!("unknown X button {}", button);
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

impl Stream for XContext {
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
}
