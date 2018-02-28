extern crate input_linux as input;
#[macro_use]
extern crate serde_derive;
extern crate serde;

use std::collections::HashMap;
use input::Key;

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

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ConfigQemu {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ga_socket: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qmp_socket: Option<String>,
    #[serde(default)]
    pub comm: ConfigQemuComm,
    #[serde(default)]
    pub driver: ConfigQemuDriver,
}

#[derive(Debug, Copy, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigQemuComm {
    Qemucomm,
    QMP,
    Console,
}

impl Default for ConfigQemuComm {
    fn default() -> Self {
        ConfigQemuComm::Qemucomm
    }
}

#[derive(Debug, Copy, Clone, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ConfigQemuDriver {
    InputLinux,
    Virtio,
}

impl Default for ConfigQemuDriver {
    fn default() -> Self {
        ConfigQemuDriver::InputLinux
    }
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
    Poweroff,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ConfigGrab {
    XCore,
    XDevice {
        devices: Vec<String>,
    },
    Evdev {
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
