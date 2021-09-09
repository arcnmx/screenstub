use std::{io, num::NonZeroU32};
use std::collections::BTreeMap;
use enumflags2::BitFlags;
use input_linux::RelativeAxis;
use xproto::protocol::*;
use xproto::protocol::xinput::XIEventMask;
use screenstub_config::ConfigInputEvent;
use crate::{context::XSink, events::{XInputEvent, XInputEventData}};

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct XInputVersion {
    major: u16,
    minor: u16,
}

impl XInputVersion {
    /// DeviceKeyPress, DeviceMotion, etc. events
    pub const _1_0: Self = Self::new(1, 0);
    /// DeviceChange/Presence events
    pub const _1_4: Self = Self::new(1, 0);
    /// DevicePropertyNotify
    pub const _1_5: Self = Self::new(1, 0);
    /// KeyPress, Motion, etc events
    pub const _2_0: Self = Self::new(2, 0);
    /// Raw press/motion events
    pub const _2_1: Self = Self::new(2, 1);
    /// Touch events
    pub const _2_2: Self = Self::new(2, 2);
    /// Pointer barrier events
    pub const _2_3: Self = Self::new(2, 3);

    pub const fn new(major: u16, minor: u16) -> Self {
        Self {
            major,
            minor,
        }
    }

    pub const fn value(&self) -> u32 {
        (self.major as u32) << 16 | self.minor as u32
    }

    pub async fn query(sink: &mut XSink, xinput: &xcore::QueryExtensionReply) -> io::Result<Option<Self>> {
        match sink.execute(xinput::XIQueryVersionRequest {
            major_opcode: xinput.major_opcode,
            major_version: xinput::XIQueryVersionRequest::EXTENSION_INFO.extension.major_version,
            minor_version: xinput::XIQueryVersionRequest::EXTENSION_INFO.extension.minor_version,
        }).await.await {
            Ok(v) => return Ok(Some(v.into())),
            Err(_) => (),
        }

        let xiversion = sink.execute(xinput::GetExtensionVersionRequest {
            major_opcode: xinput.major_opcode,
            name: xinput::XIQueryVersionRequest::EXTENSION_INFO.extension.xname[..].into(),
        }).await.await?;
        Ok(if xiversion.present {
            Some(xiversion.into())
        } else {
            None
        })
    }

    pub fn events_mask(&self, raw: bool, config_mask: BitFlags<ConfigInputEvent>) -> Vec<xproto::enums::Mask<XIEventMask, u32>> {
        let raw = raw && self >= &Self::_2_1;
        let mut masks = Vec::with_capacity(5);

        if config_mask.contains(ConfigInputEvent::Key) {
            masks.extend_from_slice(&if raw { [
                XIEventMask::RawKeyPress.into(),
                XIEventMask::RawKeyRelease.into(),
            ] } else { [
                XIEventMask::KeyPress.into(),
                XIEventMask::KeyRelease.into(),
            ] });
        }

        if config_mask.contains(ConfigInputEvent::Button) {
            masks.extend_from_slice(&if raw { [
                XIEventMask::RawButtonPress.into(),
                XIEventMask::RawButtonRelease.into(),
            ] } else { [
                XIEventMask::ButtonPress.into(),
                XIEventMask::ButtonRelease.into(),
            ] });
        }

        let has_motion = if config_mask.contains(ConfigInputEvent::Absolute) {
            masks.push(XIEventMask::Motion.into());
            true
        } else {
            false
        };
        if config_mask.intersects(ConfigInputEvent::Relative | ConfigInputEvent::Absolute) {
            if raw {
                masks.push(XIEventMask::RawMotion.into());
            } else if !has_motion {
                masks.push(XIEventMask::Motion.into());
            }
        }

        masks
    }
}

impl From<xinput::GetExtensionVersionReply> for XInputVersion {
    fn from(v: xinput::GetExtensionVersionReply) -> Self {
        Self::new(v.server_major, v.server_minor)
    }
}

impl From<xinput::XIQueryVersionReply> for XInputVersion {
    fn from(v: xinput::XIQueryVersionReply) -> Self {
        Self::new(v.major_version, v.minor_version)
    }
}

#[derive(Debug)]
pub struct XInput {
    pub min_keycode: u8,
    pub keys: xcore::GetKeyboardMappingReply,
    pub mods: xcore::GetModifierMappingReply,
    pub valuators: BTreeMap<(xinput::DeviceId, u16), xinput::DeviceClassDataValuator>,
}

impl XInput {
    pub fn new(min_keycode: u8, keys: xcore::GetKeyboardMappingReply, mods: xcore::GetModifierMappingReply) -> Self {
        Self {
            min_keycode,
            keys,
            mods,
            valuators: Default::default(),
        }
    }

    pub fn process_event_buttonpress(&self, event: &xcore::ButtonPressEvent) -> XInputEvent {
        self.process_event_buttonpress_internal(event, true)
    }

    pub fn process_event_buttonrelease(&self, event: &xcore::ButtonReleaseEvent) -> XInputEvent {
        self.process_event_buttonpress_internal(&event.0, false)
    }

    fn process_event_buttonpress_internal(&self, event: &xcore::ButtonPressEvent, pressed: bool) -> XInputEvent {
        XInputEvent {
            time: event.time,
            data: XInputEventData::Button {
                pressed,
                button: event.detail,
                state: event.state.get(),
            },
        }
    }

    pub fn process_event_keypress(&self, event: &xcore::KeyPressEvent) -> XInputEvent {
        self.process_event_keypress_internal(event, true)
    }

    pub fn process_event_keyrelease(&self, event: &xcore::KeyReleaseEvent) -> XInputEvent {
        self.process_event_keypress_internal(&event.0, false)
    }

    fn process_event_keypress_internal(&self, event: &xcore::KeyPressEvent, pressed: bool) -> XInputEvent {
        self.process_event_keypress_internal_common(event.time, event.detail, event.state.into(), pressed)
    }

    fn process_event_keypress_internal_common(&self, time: xcore::Timestamp, detail: u8, state: BitFlags<xcore::KeyButMask>, pressed: bool) -> XInputEvent {
        let keycode = self.keycode(detail);
        let keysym = self.keysym(keycode);

        XInputEvent {
            time,
            data: XInputEventData::Key {
                pressed,
                keycode,
                keysym: keysym.map(|k| k.get()),
                state,
            },
        }
    }

    pub fn process_event_motion(&self, event: &xcore::MotionNotifyEvent) -> XInputEvent {
        XInputEvent {
            time: event.time,
            data: XInputEventData::Mouse {
                x: event.event_x,
                y: event.event_y,
            },
        }
    }

    pub fn process_event_xi2_motion_raw<'a, 'e: 'a>(&'a self, event: &'e xinput::RawMotionEvent, mask: BitFlags<ConfigInputEvent>) -> impl Iterator<Item=XInputEvent> + 'a {
        let relative = mask.contains(ConfigInputEvent::Relative);
        event.axisvalues_raw().filter_map(move |(number, axisvalue)|
            self.process_event_xi2_motion_internal(event.time.value(), event.deviceid.value(), number, axisvalue, relative)
        )
    }

    pub fn process_event_xi2_motion<'a, 'e: 'a>(&'a self, event: &'e xinput::MotionEvent, mask: BitFlags<ConfigInputEvent>) -> impl Iterator<Item=XInputEvent> + 'a {
        let relative = mask.contains(ConfigInputEvent::Relative);
        event.axisvalues().filter_map(move |(number, axisvalue)|
            self.process_event_xi2_motion_internal(event.time.value(), event.deviceid.value(), number, axisvalue, relative)
        ).chain(if !relative {
            Some(XInputEvent {
                time: event.time.value(),
                data: XInputEventData::Mouse {
                    x: event.event_x(),
                    y: event.event_y(),
                },
            })
        } else { None })
    }

    fn process_event_xi2_motion_internal(&self, time: xcore::Timestamp, deviceid: xinput::DeviceId, axisnumber: u16, value: xinput::Fp3232, relative: bool) -> Option<XInputEvent> {
        // TODO: could be relative or abs?
        // TODO: figure out which axis are scroll wheels via ScrollClass - there are multiple entries per valuator?
        let valuator = match self.valuators.get(&(deviceid, axisnumber)) {
            Some(val) => val,
            None => {
                log::warn!("Unknown valuator {} for device {}", axisnumber, deviceid);
                return None
            },
        };
        match valuator.mode.get() {
            xinput::ValuatorMode::Relative if relative => Some({
                let axis = match axisnumber {
                    // TODO: match by label instead? Are these indexes fixed?
                    0 => RelativeAxis::X,
                    1 => RelativeAxis::Y,
                    2 => RelativeAxis::Wheel,
                    3 => RelativeAxis::HorizontalWheel,
                    _ => unimplemented!(),
                };
                XInputEvent {
                    time,
                    data: XInputEventData::MouseRelative {
                        axis,
                        value,
                    },
                }
            }),
            xinput::ValuatorMode::Absolute if !relative =>
                unimplemented!(),
            _ => None, // TODO: handle scrolling?
        }
    }

    pub fn process_event_xi2_keypress(&self, event: &xinput::KeyPressEvent) -> XInputEvent {
        self.process_event_xi2_keypress_internal(event, true)
    }

    pub fn process_event_xi2_keyrelease(&self, event: &xinput::KeyReleaseEvent) -> XInputEvent {
        self.process_event_xi2_keypress_internal(&event.0, false)
    }

    fn process_event_xi2_keypress_internal(&self, event: &xinput::KeyPressEvent, pressed: bool) -> XInputEvent {
        // TODO: process mods properly here
        self.process_event_keypress_internal_common(event.time.value(), event.detail as u8, Default::default(), pressed)
    }

    pub fn process_event_xi2_buttonpress(&self, event: &xinput::ButtonPressEvent) -> XInputEvent {
        self.process_event_xi2_buttonpress_internal(event, true)
    }

    pub fn process_event_xi2_buttonrelease(&self, event: &xinput::ButtonReleaseEvent) -> XInputEvent {
        self.process_event_xi2_buttonpress_internal(&event.0, false)
    }

    fn process_event_xi2_buttonpress_internal(&self, event: &xinput::ButtonPressEvent, pressed: bool) -> XInputEvent {
        XInputEvent {
            time: event.time.value(),
            data: XInputEventData::Button {
                pressed,
                button: event.detail as u8,
                state: Default::default(),
            },
        }
    }

    fn keycode(&self, code: u8) -> u8 {
        code - self.min_keycode
    }

    fn keysym(&self, code: u8) -> Option<NonZeroU32> {
        let modifier = 0; // TODO: ?
        match self.keys.keysyms.get(code as usize * self.keys.keysyms_per_keycode as usize + modifier) {
            Some(&keysym) => NonZeroU32::new(keysym),
            None => None,
        }
    }
}
