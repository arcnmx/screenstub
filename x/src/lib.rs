extern crate futures;
#[macro_use]
extern crate failure;
#[macro_use]
extern crate log;
pub extern crate xcb;

use std::sync::Arc;
use std::fmt;
use futures::sync::mpsc::{Sender, Receiver, TrySendError};
use futures::executor::{self, Notify, NotifyHandle};
use futures::Async;
use failure::Error;

#[derive(Debug, Clone, Copy, Default)]
pub struct XState {
    pub width: u16,
    pub height: u16,
    pub grabbed: bool,
    pub running: bool,
}

#[derive(Debug)]
pub enum XEvent {
    State(XState),
    Visible(bool),
    Focus(bool),
    UnstickGuest,
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
pub enum XRequest {
    Quit,
    UnstickHost,
    UnstickGuest,
    Grab,
    Ungrab,
}

pub struct XContext {
    conn: xcb::Connection,
    window: u32,

    keys: xcb::GetKeyboardMappingReply,
    mods: xcb::GetModifierMappingReply,
    state: XState,
    next_event: Option<xcb::GenericEvent>,

    atom_wm_state: xcb::Atom,
    atom_wm_protocols: xcb::Atom,
    atom_wm_delete_window: xcb::Atom,
    atom_net_wm_state: xcb::Atom,
    atom_net_wm_state_fullscreen: xcb::Atom,
    atom_atom: xcb::Atom,
}

pub trait SpinSendValue {
    fn skip_threshold(&self) -> Option<usize>;
}

impl SpinSendValue for Result<XEvent, Error> {
    fn skip_threshold(&self) -> Option<usize> {
        match *self {
            Ok(XEvent::Mouse { .. }) => Some(0),
            Ok(XEvent::Key { .. }) => Some(1),
            Ok(XEvent::Button { .. }) => Some(4),
            Err(..) => Some(0x20),
            _ => Some(0x10),
        }
    }
}

impl XContext {
    pub fn connect() -> Result<Self, Error> {
        let (conn, screen_num) = xcb::Connection::connect(None)?;
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

        Ok(XContext {
            atom_wm_state: xcb::intern_atom(&conn, true, "WM_STATE").get_reply()?.atom(),
            atom_wm_protocols: xcb::intern_atom(&conn, true, "WM_PROTOCOLS").get_reply()?.atom(),
            atom_wm_delete_window: xcb::intern_atom(&conn, true, "WM_DELETE_WINDOW").get_reply()?.atom(),
            atom_net_wm_state: xcb::intern_atom(&conn, true, "_NET_WM_STATE").get_reply()?.atom(),
            atom_net_wm_state_fullscreen: xcb::intern_atom(&conn, true, "_NET_WM_STATE_FULLSCREEN").get_reply()?.atom(),
            atom_atom: xcb::intern_atom(&conn, true, "ATOM").get_reply()?.atom(),

            keys: keys,
            mods: mods,
            state: Default::default(),
            next_event: None,

            conn: conn,
            window: window,
        })
    }

    pub fn map_window(&self) -> Result<(), Error> {
        xcb::change_property(&self.conn,
            xcb::PROP_MODE_REPLACE as _,
            self.window,
            self.atom_wm_protocols,
            self.atom_atom, 32,
            &[self.atom_wm_delete_window]
        ).request_check()?;

        xcb::change_property(&self.conn,
            xcb::PROP_MODE_APPEND as _,
            self.window,
            self.atom_net_wm_state,
            self.atom_atom, 32,
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
        if self.next_event.is_none() {
            if let Some(event) = self.conn.wait_for_event() {
                Ok(event)
            } else {
                Err(self.connection_error().unwrap().into())
            }
        } else {
            match self.next_event.take() {
                Some(e) => Ok(e),
                None => unreachable!(),
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
        self.state.running = false;
    }

    fn spin_send_once<T>(sender: &mut Sender<T>, value:T) -> Result<Option<T>, TrySendError<T>> {
        match sender.try_send(value) {
            Ok(..) => Ok(None),
            Err(err) => {
                if err.is_full() {
                    Ok(Some(err.into_inner()))
                } else {
                    Err(err)
                }
            },
        }
    }

    pub fn spin_send<T: fmt::Debug + SpinSendValue>(sender: &mut Sender<T>, value: T) -> Result<(), TrySendError<T>> {
        use std::thread::sleep;
        use std::time::Duration;

        trace!("X spin sending {:?}", value);

        let mut count = 0;
        let mut value = Some(value);
        while let Some(v) = match value.take() {
            None => None,
            Some(v) => Self::spin_send_once(sender, v)?,
        } {
            if count == 0 {
                warn!("failed to queue X event");
            }

            if let Some(skip) = v.skip_threshold() {
                if count >= skip {
                    warn!("spin_send timed out");
                    break
                }
            }

            if count < 0xffff {
                count += 1;
            }

            sleep(Duration::from_millis(20));

            value = Some(v);
        }

        Ok(())
    }

    pub fn xmain(recv: Receiver<XRequest>, sender: &mut Sender<Result<XEvent, Error>>) -> Result<(), Error> {
        #[derive(Clone)]
        struct NotifyVoid;
        impl Notify for NotifyVoid {
            fn notify(&self, _id: usize) { }
        }
        let notify = NotifyHandle::from(Arc::new(NotifyVoid));

        let mut xcontext = Self::connect()?;
        xcontext.state.running = true;
        xcontext.map_window()?;

        let mut recv = executor::spawn(recv);

        while xcontext.state.running {
            // poll for request
            let processed = match recv.poll_stream_notify(&notify, 0) {
                Ok(Async::Ready(None)) => break, // treat this as a request to exit?
                Ok(Async::Ready(Some(ref req))) => xcontext.process_request(req)?,
                Ok(Async::NotReady) => None,
                Err(..) => unreachable!(),
            };
            // otherwise block on x event
            let processed = match processed {
                Some(processed) => Some(processed),
                None => {
                    let event = &xcontext.pump()?;
                    xcontext.process_event(&event)?
                },
            };

            // send processed event
            if let Some(processed) = processed {
                match Self::spin_send(sender, Ok(processed)) {
                    Ok(..) => (),
                    Err(ref err) if err.is_disconnected() => break,
                    Err(err) => return Err(err.into()),
                }
            }
        }

        Ok(())
    }

    fn handle_grab_status(&self, status: u8) -> Result<(), Error> {
        if status == xcb::GRAB_STATUS_SUCCESS as _ {
            Ok(())
        } else {
            Err(format_err!("X failed to grab with status code {}", status))
        }
    }

    pub fn process_request(&mut self, request: &XRequest) -> Result<Option<XEvent>, Error> {
        trace!("processing X request {:?}", request);

        Ok(match *request {
            XRequest::Quit => {
                self.stop();
                None
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

                None

            },
            XRequest::UnstickGuest => {
                Some(XEvent::UnstickGuest)
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
                self.state.grabbed = true;
                Some(XEvent::State(self.state.clone()))
            },
            XRequest::Ungrab => {
                xcb::ungrab_keyboard(&self.conn, xcb::CURRENT_TIME).request_check()?;
                xcb::ungrab_pointer(&self.conn, xcb::CURRENT_TIME).request_check()?;
                self.state.grabbed = false;
                Some(XEvent::State(self.state.clone()))
            },
        })
    }

    pub fn process_event(&mut self, event: &xcb::GenericEvent) -> Result<Option<XEvent>, xcb::GenericError> {
        let kind = event.response_type() & !0x80;
        trace!("processing X event {}", kind);

        Ok(match kind {
            xcb::VISIBILITY_NOTIFY => {
                let event = unsafe { xcb::cast_event::<xcb::VisibilityNotifyEvent>(event) };

                match event.state() as _ {
                    xcb::VISIBILITY_FULLY_OBSCURED => {
                        Some(XEvent::Visible(false))
                    },
                    xcb::VISIBILITY_UNOBSCURED => {
                        Some(XEvent::Visible(true))
                    },
                    state => {
                        warn!("unknown visibility {}", state);
                        None
                    },
                }
            },
            xcb::CLIENT_MESSAGE => {
                let event = unsafe { xcb::cast_event::<xcb::ClientMessageEvent>(event) };

                match event.data().data32().get(0) {
                    Some(&atom) if atom == self.atom_wm_delete_window => {
                        self.stop();
                        None
                    },
                    Some(&atom) => {
                        let atom = xcb::get_atom_name(&self.conn, atom).get_reply();
                        warn!("unknown X client message {:?}",
                            atom.as_ref().map(|a| a.name()).unwrap_or("UNKNOWN")
                        );
                        None
                    },
                    None => {
                        warn!("empty client message");
                        None
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
                                Some(XEvent::Visible(false))
                            },
                            Some(&state) => {
                                info!("unknown WM_STATE {}", state);
                                None
                            },
                            None => {
                                warn!("expected WM_STATE state value");
                                None
                            },
                        }
                    },
                    atom => {
                        let atom = xcb::get_atom_name(&self.conn, atom).get_reply();
                        warn!("unknown property notify {:?}",
                            atom.as_ref().map(|a| a.name()).unwrap_or("UNKNOWN")
                        );
                        None
                    },
                }
            },
            xcb::FOCUS_OUT | xcb::FOCUS_IN => {
                Some(XEvent::Focus(kind == xcb::FOCUS_IN))
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
                        return Ok(None)
                    }
                }

                let keycode = self.keycode(event.detail());
                let keysym = self.keysym(keycode);

                Some(XEvent::Key {
                    pressed: kind == xcb::KEY_PRESS,
                    keycode: keycode,
                    keysym: if keysym == Some(0) { None } else { keysym },
                    state: event.state(),
                })
            },
            xcb::BUTTON_PRESS | xcb::BUTTON_RELEASE => {
                let event = unsafe { xcb::cast_event::<xcb::ButtonPressEvent>(event) };
                Some(XEvent::Button {
                    pressed: kind == xcb::BUTTON_PRESS,
                    button: event.detail(),
                    state: event.state(),
                })
            },
            xcb::MOTION_NOTIFY => {
                let event = unsafe { xcb::cast_event::<xcb::MotionNotifyEvent>(event) };
                Some(XEvent::Mouse {
                    x: event.event_x(),
                    y: event.event_y(),
                })
            },
            xcb::MAPPING_NOTIFY => {
                let setup = self.conn.get_setup();
                self.keys = xcb::get_keyboard_mapping(&self.conn, setup.min_keycode(), setup.max_keycode() - setup.min_keycode()).get_reply()?;
                self.mods = xcb::get_modifier_mapping(&self.conn).get_reply()?;

                None
            },
            xcb::CONFIGURE_NOTIFY => {
                let event = unsafe { xcb::cast_event::<xcb::ConfigureNotifyEvent>(event) };
                self.state.width = event.width();
                self.state.height = event.height();
                Some(XEvent::State(self.state.clone()))
            },
            _ => {
                warn!("unknown X event {}", event.response_type());
                None
            },
        })
    }
}
