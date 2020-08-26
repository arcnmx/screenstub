extern crate input_linux as input;

use std::collections::HashMap;
use std::time::Duration;
use std::fmt;
use enumflags2::BitFlags;
use serde::{Serialize, Deserialize};
use input::{Key, InputEvent, EventRef};

pub mod keymap;

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Config {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub screens: Vec<ConfigScreen>,

    #[serde(default)]
    pub qemu: ConfigQemu,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hotkeys: Vec<ConfigHotkey>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub key_remap: HashMap<Key, Key>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exit_events: Vec<ConfigEvent>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ConfigScreen {
    #[serde(default)]
    pub monitor: ConfigMonitor,
    #[serde(default)]
    pub guest_source: ConfigSource,
    #[serde(default)]
    pub host_source: ConfigSource,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ddc: Option<ConfigDdc>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub x_instance: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigDdcMethod {
    Ddc,
    Libddcutil,
    Ddcutil,
    Exec(Vec<String>),
    GuestExec(Vec<String>),
    GuestWait,
}

impl ConfigDdcMethod {
    #[cfg(all(not(feature = "with-ddc"), feature = "with-ddcutil"))]
    fn default_host() -> Vec<Self> {
        vec![ConfigDdcMethod::Libddcutil]
    }

    #[cfg(feature = "with-ddc")]
    fn default_host() -> Vec<Self> {
        vec![ConfigDdcMethod::Ddc]
    }

    #[cfg(not(any(feature = "with-ddcutil", feature = "with-ddc")))]
    fn default_host() -> Vec<Self> {
        Vec::new()
    }

    fn default_guest() -> Vec<Self> {
        Self::default_host()
    }
}

impl Default for ConfigDdc {
    fn default() -> Self {
        Self {
            host: ConfigDdcMethod::default_host(),
            guest: ConfigDdcMethod::default_guest(),
            minimal_delay: Self::default_delay(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ConfigDdc {
    #[serde(default = "ConfigDdcMethod::default_host")]
    pub host: Vec<ConfigDdcMethod>,
    #[serde(default = "ConfigDdcMethod::default_guest")]
    pub guest: Vec<ConfigDdcMethod>,
    #[serde(default = "ConfigDdc::default_delay", with = "humantime_serde")]
    pub minimal_delay: Duration,
}

impl ConfigDdc {
    fn default_delay() -> Duration {
        Duration::from_millis(100)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ConfigQemu {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ga_socket: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qmp_socket: Option<String>,

    #[serde(default)]
    pub driver: Option<ConfigQemuDriver>,
    #[serde(default)]
    pub keyboard_driver: Option<ConfigQemuDriver>,
    #[serde(default)]
    pub relative_driver: Option<ConfigQemuDriver>,
    #[serde(default)]
    pub absolute_driver: Option<ConfigQemuDriver>,

    #[serde(default = "ConfigQemuRouting::qmp")]
    pub routing: ConfigQemuRouting,
}

impl Default for ConfigQemu {
    fn default() -> Self {
        ConfigQemu {
            ga_socket: Default::default(),
            qmp_socket: Default::default(),
            driver: Default::default(),
            keyboard_driver: Default::default(),
            relative_driver: Default::default(),
            absolute_driver: Default::default(),
            routing: ConfigQemuRouting::Qmp,
        }
    }
}

impl ConfigQemu {
    pub fn keyboard_driver(&self) -> ConfigQemuDriver {
        self.keyboard_driver
            .or(self.driver)
            .unwrap_or(ConfigQemuDriver::Ps2)
    }

    pub fn relative_driver(&self) -> ConfigQemuDriver {
        self.relative_driver
            .or(self.driver)
            .unwrap_or(ConfigQemuDriver::Usb)
    }

    pub fn absolute_driver(&self) -> ConfigQemuDriver {
        self.absolute_driver
            .or(self.driver)
            .unwrap_or(ConfigQemuDriver::Usb)
    }
}

#[derive(Debug, Copy, Clone, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ConfigQemuDriver {
    Ps2,
    Usb,
    Virtio,
}

#[derive(Debug, Copy, Clone, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ConfigQemuRouting {
    InputLinux,
    VirtioHost,
    Spice,
    Qmp,
}

impl ConfigQemuRouting {
    fn qmp() -> Self { ConfigQemuRouting::Qmp }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ConfigHotkey {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub triggers: Vec<Key>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub modifiers: Vec<Key>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<ConfigEvent>,
    #[serde(default)]
    pub on_release: bool,
    #[serde(default)]
    pub global: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigEvent {
    Exec(Vec<String>),
    GuestExec(Vec<String>),
    GuestWait,
    ShowHost,
    ShowGuest,
    ToggleShow,
    ToggleGrab(ConfigGrab),
    Grab(ConfigGrab),
    Ungrab(ConfigGrabMode),
    UnstickHost,
    UnstickGuest,
    Shutdown,
    Reboot,
    Exit,
}

#[derive(Debug, Copy, Clone, PartialOrd, Ord, PartialEq, Eq, Hash, Deserialize, Serialize, BitFlags)]
#[repr(u8)]
#[serde(rename_all = "snake_case")]
pub enum ConfigInputEvent {
    Key = 0x01,
    Button = 0x02,
    Relative = 0x04,
    Absolute = 0x08,
    Misc = 0x10,
    Switch = 0x20,
    Led = 0x40,
    Sound = 0x80,
}

impl ConfigInputEvent {
    pub fn from_event(e: &InputEvent) -> Option<Self> {
        match EventRef::new(e) {
            Ok(EventRef::Key(key)) if key.key.is_key() => Some(ConfigInputEvent::Key),
            Ok(EventRef::Key(key)) if key.key.is_button() => Some(ConfigInputEvent::Button),
            Ok(EventRef::Relative(..)) => Some(ConfigInputEvent::Relative),
            Ok(EventRef::Absolute(..)) => Some(ConfigInputEvent::Absolute),
            Ok(EventRef::Misc(..)) => Some(ConfigInputEvent::Misc),
            Ok(EventRef::Switch(..)) => Some(ConfigInputEvent::Switch),
            Ok(EventRef::Led(..)) => Some(ConfigInputEvent::Led),
            Ok(EventRef::Sound(..)) => Some(ConfigInputEvent::Sound),
            _ => None,
        }
    }

    pub fn event_matches(&self, e: EventRef) -> bool {
        match (*self, e) {
            (ConfigInputEvent::Key, EventRef::Key(key)) if key.key.is_key() => true,
            (ConfigInputEvent::Button, EventRef::Key(key)) if key.key.is_button() => true,
            (ConfigInputEvent::Relative, EventRef::Relative(..)) => true,
            (ConfigInputEvent::Absolute, EventRef::Absolute(..)) => true,
            (ConfigInputEvent::Misc, EventRef::Misc(..)) => true,
            (ConfigInputEvent::Switch, EventRef::Switch(..)) => true,
            (ConfigInputEvent::Led, EventRef::Led(..)) => true,
            (ConfigInputEvent::Sound, EventRef::Sound(..)) => true,
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ConfigGrab {
    XCore,
    XInput,
    XDevice {
        devices: Vec<String>,
    },
    Evdev {
        #[serde(default)]
        exclusive: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        new_device_name: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        xcore_ignore: Vec<ConfigInputEvent>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        evdev_ignore: Vec<ConfigInputEvent>,
        devices: Vec<String>,
    },
}

impl ConfigGrab {
    pub fn mode(&self) -> ConfigGrabMode {
        match *self {
            ConfigGrab::XCore => ConfigGrabMode::XCore,
            ConfigGrab::XInput => ConfigGrabMode::XInput,
            ConfigGrab::XDevice { .. } => ConfigGrabMode::XDevice,
            ConfigGrab::Evdev { .. } => ConfigGrabMode::Evdev,
        }
    }
}

impl Default for ConfigGrab {
    fn default() -> Self {
        ConfigGrab::XCore
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ConfigGrabMode {
    Evdev,
    XDevice,
    XCore,
    XInput,
}

impl Default for ConfigGrabMode {
    fn default() -> Self {
        ConfigGrabMode::XCore
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ConfigMonitor {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manufacturer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serial: Option<String>,
    //#[serde(default, skip_serializing_if = "Option::is_none")]
    // pub path: Option<DisplayPath>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub xrandr_name: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ConfigSource {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<ConfigSourceName>,
}

impl ConfigSource {
    pub fn value(&self) -> Option<u8> {
        self.value.or(self.name.as_ref().map(ConfigSourceName::value))
    }
}

#[derive(Debug, Copy, Clone, Deserialize, Serialize)]
#[repr(u8)]
pub enum ConfigSourceName {
    #[serde(rename = "Analog-1")]
    Analog1 = 0x01,
    #[serde(rename = "Analog-2")]
    Analog2 = 0x02,
    #[serde(rename = "DVI-1")]
    DVI1 = 0x03,
    #[serde(rename = "DVI-2")]
    DVI2 = 0x04,
    #[serde(rename = "Composite-1")]
    Composite1 = 0x05,
    #[serde(rename = "Composite-2")]
    Composite2 = 0x06,
    #[serde(rename = "S-video-1")]
    SVideo1 = 0x07,
    #[serde(rename = "S-video-2")]
    SVideo2 = 0x08,
    #[serde(rename = "Tuner-1")]
    Tuner1 = 0x09,
    #[serde(rename = "Tuner-2")]
    Tuner2 = 0x0a,
    #[serde(rename = "Tuner-3")]
    Tuner3 = 0x0b,
    #[serde(rename = "Component-1")]
    Component1 = 0x0c,
    #[serde(rename = "Component-2")]
    Component2 = 0x0d,
    #[serde(rename = "Component-3")]
    Component3 = 0x0e,
    #[serde(rename = "DisplayPort-1")]
    DisplayPort1 = 0x0f,
    #[serde(rename = "DisplayPort-2")]
    DisplayPort2 = 0x10,
    #[serde(rename = "HDMI-1")]
    HDMI1 = 0x11,
    #[serde(rename = "HDMI-2")]
    HDMI2 = 0x12,
}

impl fmt::Display for ConfigSourceName {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.serialize(f)
    }
}

impl ConfigSourceName {
    pub fn value(&self) -> u8 {
        *self as _
    }

    pub fn from_value(value: u8) -> Option<Self> {
        match value {
            1..=0x12 => Some(unsafe { Self::from_value_unchecked(value) }),
            _ => None,
        }
    }

    pub unsafe fn from_value_unchecked(value: u8) -> Self {
        core::mem::transmute(value)
    }
}
