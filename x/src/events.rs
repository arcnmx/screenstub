use input_linux::{EventTime, InputEvent, SynchronizeEvent, KeyEvent, KeyState, Key, RelativeAxis, RelativeEvent, AbsoluteAxis, AbsoluteEvent};
use std::num::{NonZeroU64, NonZeroU16, NonZeroU32};
use enumflags2::BitFlags;
use xproto::protocol::{xcore, xinput};
use log::warn;
use crate::{XEvent, Rect, Motion, Bounds};

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
        value: xinput::Fp3232,
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
pub struct Position {
    pub width: u16,
    pub height: u16,
    pub last_x: i32,
    pub last_y: i32,
    pub bounds: Rect,
}

impl Default for Position {
    fn default() -> Self {
        Self::new()
    }
}

impl Position {
    pub fn new() -> Self {
        Self {
            width: Default::default(),
            height: Default::default(),
            last_x: -1,
            last_y: -1,
            bounds: Default::default(),
        }
    }

    fn scale_absolute(bounds: &Bounds, dim: NonZeroU16, value: i16) -> u32 {
        let value = (value.max(0) as u16).min(dim.get()) as u32;
        bounds.upper.min(bounds.lower + value * bounds.size / NonZeroU32::from(dim))
    }

    fn convert_absolute<'a>(&'a mut self, x: i16, y: i16) -> impl Iterator<Item=(AbsoluteAxis, i32)> + 'a {
        use std::iter::once;

        once(
            (AbsoluteAxis::X, &self.bounds.x, self.width, x, &mut self.last_x)
        ).chain(once(
            (AbsoluteAxis::Y, &self.bounds.y, self.height, y, &mut self.last_y)
        )).filter_map(|(axis, bounds, dim, value, last)| {
            let dim = NonZeroU16::new(dim)?;
            let value = Self::scale_absolute(bounds, dim, value) as i32;
            if value == *last {
                None
            } else {
                *last = value;
                Some((axis, value))
            }
        })
    }
}

#[derive(Debug)]
pub struct XEventQueue {
    event_queue: Vec<XEvent>,
    pub motion: Motion,
    pub position: Position,
    resolution: NonZeroU64,
}

impl Default for XEventQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl XEventQueue {
    pub fn new() -> Self {
        Self {
            event_queue: Default::default(),
            position: Default::default(),
            motion: Default::default(),
            resolution: unsafe { NonZeroU64::new_unchecked(1) },
        }
    }

    pub fn set_size(&mut self, width: u16, height: u16) {
        self.position.width = width;
        self.position.height = height;
    }

    pub fn set_bounds(&mut self, bounds: Rect) {
        self.position.bounds = bounds;
    }

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

    pub fn commit(&mut self) {
        let sync = self.synchronize_start();
        self.commit_pending();
        self.synchronize_end(sync);
    }

    fn commit_pending(&mut self) {
        let time = Default::default();
        let values = [
            (RelativeAxis::X, self.motion.x.round_up(self.resolution)),
            (RelativeAxis::Y, self.motion.y.round_up(self.resolution)),
        ];
        let values = values.iter().filter_map(|&(axis, value)| value.map(|v| (axis, v)))
            .map(|(axis, value)| XEvent::Input(RelativeEvent::new(time, axis, value as i32).into()));
        self.event_queue.extend(values);
    }

    fn synchronize_start(&self) -> usize {
        self.event_queue.len()
    }

    fn synchronize_end(&mut self, state: usize) {
        let time = Default::default();
        if self.synchronize_start() != state {
            self.event_queue.push(XEvent::Input(SynchronizeEvent::report(time).into()));
        }
    }

    fn convert_x_events(&mut self, e: &XInputEvent) {
        //let time = Self::event_time(e.time);
        let time = Default::default();
        let sync = self.synchronize_start();
        match e.data {
            XInputEventData::Mouse { x, y } => {
                self.commit_pending();
                self.event_queue.extend(self.position.convert_absolute(x, y)
                    .map(|(axis, value)| AbsoluteEvent::new(
                        time,
                        axis,
                        value,
                    )).map(|e| XEvent::Input(e.into()))
                );
            },
            XInputEventData::MouseRelative { axis, value } => {
                let committed = match axis {
                    RelativeAxis::X => self.motion.x.append(value.fixed_point(), self.resolution),
                    RelativeAxis::Y => self.motion.y.append(value.fixed_point(), self.resolution),
                    _ => Some(value.integral as i64),
                };
                if let Some(value) = committed {
                    self.event_queue.push(XEvent::Input(RelativeEvent::new(time, axis, value as i32).into()));
                }
            },
            XInputEventData::Button { pressed, button, state: _ } => {
                self.commit_pending();
                if let Some(button) = Self::x_button(button) {
                    self.event_queue.push(XEvent::Input(Self::key_event(time, button, pressed).into()));
                } else {
                    warn!("unknown X button {:?}", button);
                }
            },
            XInputEventData::Key { pressed, keycode, keysym, state: _ } => {
                self.commit_pending();
                if let Some(key) = Self::x_keycode(keycode) {
                    self.event_queue.push(XEvent::Input(Self::key_event(time, key, pressed).into()));
                } else {
                    warn!("unknown X keycode {} keysym {:?}", keycode, keysym);
                }
            },
        }
        self.synchronize_end(sync);
    }
}
