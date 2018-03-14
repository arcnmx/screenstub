extern crate input_linux as input;
#[macro_use]
extern crate serde_derive;
extern crate serde;

use std::collections::HashMap;
use input::{Key, InputEvent, EventRef};

pub type Config = Vec<ConfigScreen>;

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ConfigScreen {
    #[serde(default)]
    pub monitor: ConfigMonitor,
    #[serde(default)]
    pub guest_source: ConfigInput,
    #[serde(default)]
    pub host_source: ConfigInput,

    #[serde(default)]
    pub ddc: ConfigDdc,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hotkeys: Vec<ConfigHotkey>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub key_remap: HashMap<Key, Key>,

    #[serde(default)]
    pub qemu: ConfigQemu,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exit_events: Vec<ConfigEvent>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigDdcHost {
    None,
    #[cfg(feature = "with-ddcutil")]
    Libddcutil,
    Ddcutil,
    Exec(Vec<String>),
}

impl Default for ConfigDdcHost {
    #[cfg(feature = "with-ddcutil")]
    fn default() -> Self {
        ConfigDdcHost::Libddcutil
    }

    #[cfg(not(feature = "with-ddcutil"))]
    fn default() -> Self {
        ConfigDdcHost::None
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigDdcGuest {
    None,
    GuestExec(Vec<String>),
    Exec(Vec<String>),
}

impl Default for ConfigDdcGuest {
    fn default() -> Self {
        ConfigDdcGuest::None
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ConfigDdc {
    #[serde(default)]
    pub host: ConfigDdcHost,
    #[serde(default)]
    pub guest: ConfigDdcGuest,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ConfigQemu {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ga_socket: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qmp_socket: Option<String>,

    #[serde(default)]
    pub driver: Option<ConfigQemuDriver>,
    #[serde(default = "ConfigQemuDriver::ps2")]
    pub keyboard_driver: ConfigQemuDriver,
    #[serde(default = "ConfigQemuDriver::usb")]
    pub relative_driver: ConfigQemuDriver,
    #[serde(default = "ConfigQemuDriver::usb")]
    pub absolute_driver: ConfigQemuDriver,

    #[serde(default = "ConfigQemuRouting::qmp")]
    pub routing: ConfigQemuRouting,
}

impl Default for ConfigQemu {
    fn default() -> Self {
        ConfigQemu {
            ga_socket: Default::default(),
            qmp_socket: Default::default(),
            driver: Default::default(),
            keyboard_driver: ConfigQemuDriver::Ps2,
            relative_driver: ConfigQemuDriver::Usb,
            absolute_driver: ConfigQemuDriver::Usb,
            routing: ConfigQemuRouting::Qmp,
        }
    }
}

#[derive(Debug, Copy, Clone, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ConfigQemuDriver {
    Ps2,
    Usb,
    Virtio,
}

impl ConfigQemuDriver {
    fn ps2() -> Self { ConfigQemuDriver::Ps2 }
    fn usb() -> Self { ConfigQemuDriver::Usb }
}

#[derive(Debug, Copy, Clone, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ConfigQemuRouting {
    InputLinux,
    VirtioHost,
    //Spice,
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

#[derive(Debug, Copy, Clone, PartialOrd, Ord, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigInputEvent {
    Key,
    Button,
    Relative,
    Absolute,
    Misc,
    Switch,
    Led,
    Sound,
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
}

impl Default for ConfigGrabMode {
    fn default() -> Self {
        ConfigGrabMode::XCore
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ConfigMonitor {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manufacturer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serial: Option<String>,
    //#[serde(default, skip_serializing_if = "Option::is_none")]
    // pub path: Option<DisplayPath>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ConfigInput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}
