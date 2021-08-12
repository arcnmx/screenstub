use input_linux::{EventTime, InputEvent, SynchronizeEvent, KeyEvent, KeyState, Key, RelativeAxis, RelativeEvent, AbsoluteAxis, AbsoluteEvent};
use enumflags2::BitFlags;
use xproto::protocol::xcore;
use log::warn;
use crate::{XEvent, Rect};

#[derive(Debug)]
pub struct XInputEvent {
    pub time: u32,
    pub data: XInputEventData,
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

#[derive(Debug, Default)]
pub struct XEventQueue {
    event_queue: Vec<XEvent>,
    pub width: u16,
    pub height: u16,
    pub bounds: Rect,
}

impl XEventQueue {
    pub fn push(&mut self, event: XEvent) {
        self.event_queue.push(event)
    }

    pub fn push_x_event(&mut self, event: &XInputEvent) {
        self.convert_x_events(event)
    }

    pub fn pop(&mut self) -> Option<XEvent> {
        if self.event_queue.is_empty() {
            None
        } else {
            Some(self.event_queue.remove(0))
        }
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

    fn convert_x_events(&mut self, e: &XInputEvent) {
        //let time = Self::event_time(e.time);
        let time = Default::default();
        match e.data {
            XInputEventData::Mouse { x, y } => {
                self.event_queue.extend([
                    (self.width, x, AbsoluteAxis::X, self.bounds.x),
                    (self.height, y, AbsoluteAxis::Y, self.bounds.y),
                ].iter()
                    .filter(|&&(dim, new, _, _)| dim != 0)
                    .map(|&(dim, new, axis, bounds)| (dim, (new.max(0) as u16).min(dim), axis, bounds))
                    .map(|(dim, new, axis, bounds)| AbsoluteEvent::new(
                        time,
                        axis,
                        bounds.upper.min(bounds.lower + new as i32 * bounds.size / dim as i32),
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
}
