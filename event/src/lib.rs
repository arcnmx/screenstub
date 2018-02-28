#[macro_use]
extern crate log;
extern crate screenstub_x as x;
extern crate input_linux as input;

use std::collections::HashMap;
use std::rc::Rc;
use std::{slice, iter};
use input::{
    EventRef, EventMut, InputEvent, SynchronizeEvent, SynchronizeKind,
    KeyEvent, Key, KeyState,
    AbsoluteEvent, AbsoluteAxis,
    Bitmask,
};
use x::{XState, XEvent, xcb};

#[derive(Debug)]
pub enum UserEvent {
    ShowGuest,
    ShowHost,
    UnstickGuest,
    UnstickHost,
}

#[derive(Debug)]
pub struct Hotkey<U> {
    triggers: Vec<Key>,
    modifiers: Vec<Key>,
    events: Vec<Rc<U>>,
}

impl<U> Hotkey<U> {
    pub fn new<T: IntoIterator<Item=Key>, M: IntoIterator<Item=Key>, E: IntoIterator<Item=U>>(triggers: T, modifiers: M, events: E) -> Self {
        Hotkey {
            triggers: triggers.into_iter().collect(),
            modifiers: modifiers.into_iter().collect(),
            events: events.into_iter().map(Rc::new).collect(),
        }
    }
    pub fn keys(&self) -> iter::Cloned<iter::Chain<slice::Iter<Key>, slice::Iter<Key>>> {
        self.triggers.iter().chain(self.modifiers.iter()).cloned()
    }
}

#[derive(Debug)]
pub struct Events<U> {
    xstate: XState,
    mouse_x: i16,
    mouse_y: i16,
    triggers_press: HashMap<Key, Vec<Rc<Hotkey<U>>>>,
    triggers_release: HashMap<Key, Vec<Rc<Hotkey<U>>>>,
    remap: HashMap<Key, Key>,
    keys: Bitmask<Key>,
}

#[derive(Debug)]
pub enum ProcessedXEvent {
    UserEvent(UserEvent),
    InputEvent(InputEvent),
}

impl From<UserEvent> for ProcessedXEvent {
    fn from(e: UserEvent) -> Self {
        ProcessedXEvent::UserEvent(e)
    }
}

impl<I: Into<InputEvent>> From<I> for ProcessedXEvent {
    fn from(e: I) -> Self {
        ProcessedXEvent::InputEvent(e.into())
    }
}

impl<U> Events<U> {
    pub fn new() -> Self {
        Events {
            xstate: Default::default(),
            mouse_x: -1,
            mouse_y: -1,
            triggers_press: Default::default(),
            triggers_release: Default::default(),
            remap: Default::default(),
            keys: Default::default(),
        }
    }

    pub fn add_hotkey(&mut self, hotkey: Hotkey<U>, on_press: bool) {
        let hotkey = Rc::new(hotkey);
        for &key in &hotkey.triggers {
            if on_press {
                &mut self.triggers_press
            } else {
                &mut self.triggers_release
            }.entry(key).or_insert(Default::default()).push(hotkey.clone())
        }
    }

    pub fn add_remap(&mut self, from: Key, to: Key) {
        self.remap.insert(from, to);
    }

    pub fn x_button(&self, button: xcb::Button) -> Option<Key> {
        match button as _ {
            xcb::BUTTON_INDEX_1 => Some(Key::ButtonLeft),
            xcb::BUTTON_INDEX_2 => Some(Key::ButtonMiddle),
            xcb::BUTTON_INDEX_3 => Some(Key::ButtonRight),
            xcb::BUTTON_INDEX_4 => Some(Key::ButtonWheel), // Key::ButtonGearDown
            xcb::BUTTON_INDEX_5 => Some(Key::ButtonGearUp),
            // also map Key::ButtonSide, Key::ButtonExtra? qemu input-linux doesn't support fwd/back, but virtio probably does
            _ => None,
        }
    }

    pub fn x_keycode(&self, key: xcb::Keycode) -> Option<Key> {
        match Key::from_code(key as _) {
            Ok(code) => Some(code),
            Err(..) => None,
        }
    }

    pub fn x_keysym(&self, _key: xcb::Keysym) -> Option<Key> {
        unimplemented!()
    }

    pub fn map_input_event(&mut self, mut e: InputEvent) -> InputEvent {
        let key = match EventMut::new(&mut e) {
            Ok(EventMut::Key(key)) => if let Some(remap) = self.remap.get(&key.key) {
                key.key = *remap;
            },
            _ => (),
        };

        e
    }

    pub fn process_input_event(&mut self, e: &InputEvent) -> Vec<Rc<U>> {
        match EventRef::new(e) {
            Ok(e) => self.process_input_event_(e),
            Err(err) => {
                warn!("Unable to parse input event {:?} due to {:?}", e, err);
                Default::default()
            },
        }
    }

    fn process_input_event_(&mut self, e: EventRef) -> Vec<Rc<U>> {
        match e {
            EventRef::Key(key) => {
                let state = key.key_state();

                let hotkeys = match state {
                    KeyState::Pressed => self.triggers_press.get(&key.key),
                    KeyState::Released => self.triggers_release.get(&key.key),
                    _ => None,
                };

                match state {
                    KeyState::Pressed => self.keys.set(key.key),
                    _ => (),
                }

                let events = if let Some(hotkeys) = hotkeys {
                    hotkeys.iter()
                        .filter(|h| h.keys().all(|k| self.keys.get(k)))
                        .filter(|h| h.triggers.contains(&key.key))
                        .flat_map(|h| h.events.iter().cloned())
                        .collect()
                } else {
                    Default::default()
                };

                match state {
                    KeyState::Pressed => (),
                    KeyState::Released => self.keys.clear(key.key),
                    state => warn!("Unknown key state {:?}", state),
                }

                events
            },
            _ => Default::default(),
        }
    }

    fn sync_report() -> InputEvent {
        SynchronizeEvent::new(Default::default(), SynchronizeKind::Report, 0).into()
    }

    fn key_state(pressed: bool) -> i32 {
        match pressed {
            true => KeyState::Pressed.into(),
            false => KeyState::Released.into(),
        }
    }

    fn key_event(key: Key, pressed: bool) -> Vec<ProcessedXEvent> {
        vec![
            KeyEvent::new(Default::default(), key, Self::key_state(pressed)).into(),
            Self::sync_report().into(),
        ]
    }

    pub fn process_x_event(&mut self, e: &XEvent) -> Vec<ProcessedXEvent> {
        match *e {
            XEvent::State(state) => {
                self.xstate = state;
                Default::default()
            },
            XEvent::Visible(visible) => vec![if visible {
                UserEvent::ShowGuest.into()
            } else {
                UserEvent::ShowHost.into()
            }],
            XEvent::Focus(focus) => if !focus {
                vec![UserEvent::UnstickGuest.into()] // TODO: wtf just generate the events here!!
            } else {
                Default::default()
            },
            XEvent::UnstickGuest => {
                let res = self.unstick_events().map(From::from).collect();
                self.keys = Default::default();
                res
            },
            XEvent::Mouse { x, y } => {
                let events = [
                    (self.xstate.width, self.mouse_x, x, AbsoluteAxis::X),
                    (self.xstate.height, self.mouse_y, y, AbsoluteAxis::Y),
                ].iter()
                    .filter(|&&(dim, old, new, _)| old != new && dim != 0)
                    .map(|&(dim, _, new, axis)| (
                        dim,
                        if new < 0 {
                            0
                        } else if new as u16 > dim {
                            dim as _
                        } else {
                            new
                        },
                        axis
                    )).map(|(dim, new, axis)| AbsoluteEvent::new(
                        Default::default(),
                        axis,
                        new as i32 * 0x8000 / dim as i32,
                    ).into())
                    .chain(iter::once(Self::sync_report().into()))
                    .collect();

                self.mouse_x = x;
                self.mouse_y = y;

                events
            },
            XEvent::Button { pressed, button, .. } => {
                if let Some(button) = self.x_button(button) {
                    Self::key_event(button, pressed)
                } else {
                    warn!("unknown X button {}", button);
                    Default::default()
                }
            },
            XEvent::Key { pressed, keycode, keysym, .. } => {
                if let Some(key) = self.x_keycode(keycode) { // TODO: keysym?
                    Self::key_event(key, pressed)
                } else {
                    warn!("unknown X keycode {} keysym {:?}", keycode, keysym);
                    Default::default()
                }
            },
        }
    }

    pub fn unstick_events(&self) -> iter::Chain<iter::Map<input::bitmask::BitmaskIterator<Key>, fn(Key) -> InputEvent>, iter::Once<InputEvent>> {
        fn key_event(key: Key) -> InputEvent {
            KeyEvent::new(Default::default(), key, Events::<()>::key_state(false)).into()
        }

        self.keys.iter().map(key_event as _)
            .chain(iter::once(Self::sync_report()))
    }
}
