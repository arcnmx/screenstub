use std::collections::HashMap;
use std::sync::Mutex;
use std::{slice, iter};
use smallvec::{SmallVec, smallvec};
use input_linux::{
    EventRef, EventMut, InputEvent, SynchronizeEvent,
    KeyEvent, Key, KeyState,
    Bitmask,
};
use log::warn;
use screenstub_x::XEvent;

#[derive(Debug)]
pub enum UserEvent {
    Quit,
    ShowGuest,
    ShowHost,
    UnstickGuest,
    UnstickHost,
}

#[derive(Debug, Clone)]
pub struct Hotkey<U> {
    triggers: Vec<Key>,
    modifiers: Vec<Key>,
    events: Vec<U>,
}

impl<U> Hotkey<U> {
    pub fn new<T: IntoIterator<Item=Key>, M: IntoIterator<Item=Key>, E: IntoIterator<Item=U>>(triggers: T, modifiers: M, events: E) -> Self {
        Hotkey {
            triggers: triggers.into_iter().collect(),
            modifiers: modifiers.into_iter().collect(),
            events: events.into_iter().collect(),
        }
    }

    pub fn keys(&self) -> iter::Cloned<iter::Chain<slice::Iter<Key>, slice::Iter<Key>>> {
        self.triggers.iter().chain(self.modifiers.iter()).cloned()
    }
}

#[derive(Debug)]
pub struct Events<U> {
    triggers_press: HashMap<Key, Vec<Hotkey<U>>>,
    triggers_release: HashMap<Key, Vec<Hotkey<U>>>,
    remap: HashMap<Key, Key>,
    keys: Mutex<Bitmask<Key>>,
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
            triggers_press: Default::default(),
            triggers_release: Default::default(),
            remap: Default::default(),
            keys: Default::default(),
        }
    }

    pub fn add_hotkey(&mut self, hotkey: Hotkey<U>, on_press: bool) where U: Clone {
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

    pub fn map_input_event(&self, mut e: InputEvent) -> InputEvent {
        match EventMut::new(&mut e) {
            Ok(EventMut::Key(key)) => if let Some(remap) = self.remap.get(&key.key) {
                key.key = *remap;
            },
            _ => (),
        }

        e
    }

    pub fn process_input_event<'a>(&'a self, e: &InputEvent) -> Vec<&'a U> {
        match EventRef::new(e) {
            Ok(e) => self.process_input_event_(e),
            Err(err) => {
                warn!("Unable to parse input event {:?} due to {:?}", e, err);
                Default::default()
            },
        }
    }

    fn process_input_event_<'a>(&'a self, e: EventRef) -> Vec<&'a U> {
        match e {
            EventRef::Key(key) => {
                let state = key.value;

                let hotkeys = match state {
                    KeyState::PRESSED => self.triggers_press.get(&key.key),
                    KeyState::RELEASED => self.triggers_release.get(&key.key),
                    _ => None,
                };

                let mut keys = self.keys.lock().unwrap();
                match state {
                    KeyState::PRESSED => keys.insert(key.key),
                    _ => (),
                }

                let events = if let Some(hotkeys) = hotkeys {
                    hotkeys.iter()
                        .filter(|h| h.keys().all(|k| keys.get(k)))
                        .filter(|h| h.triggers.contains(&key.key))
                        .flat_map(|h| h.events.iter())
                        .collect()
                } else {
                    Default::default()
                };

                match state {
                    KeyState::PRESSED => (),
                    KeyState::RELEASED => keys.remove(key.key),
                    state => warn!("Unknown key state {:?}", state),
                }

                events
            },
            _ => Default::default(),
        }
    }

    pub fn process_x_event(&self, e: &XEvent) -> impl Iterator<Item=ProcessedXEvent> {
        match *e {
            XEvent::Close =>
                smallvec![UserEvent::Quit.into()],
            XEvent::Visible(visible) => smallvec![if visible {
                UserEvent::ShowGuest.into()
            } else {
                UserEvent::ShowHost.into()
            }],
            XEvent::Focus(focus) => if !focus {
                self.unstick_guest_()
            } else {
                Default::default()
            },
            XEvent::Input(e) => {
                smallvec![e.into()]
            },
        }.into_iter()
    }

    fn unstick_events_<'a>(keys: &'a Bitmask<Key>) -> impl Iterator<Item=InputEvent> + 'a {
        keys.iter().map(|key|
            KeyEvent::new(Default::default(), key, KeyState::RELEASED).into()
        ).chain(iter::once(SynchronizeEvent::report(Default::default()).into()))
    }

    pub fn unstick_guest(&self) -> impl Iterator<Item=InputEvent> + Send {
        let mut keys = self.keys.lock().unwrap();
        let res: SmallVec<[InputEvent; 4]> = Self::unstick_events_(&keys).collect();
        keys.clear();
        res.into_iter()
    }

    fn unstick_guest_(&self) -> SmallVec<[ProcessedXEvent; 4]> {
        // sad duplicate because it seems slightly more efficient :(
        let mut keys = self.keys.lock().unwrap();
        let res = Self::unstick_events_(&keys).map(From::from).collect();
        keys.clear();
        res
    }
}
