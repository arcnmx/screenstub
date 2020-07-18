use serde::{Serialize, Deserialize};
use qapi_spec::Enum;
use qapi_qmp::QKeyCode;

mod hex_opt_serde {
    use std::str::FromStr;
    use std::fmt::Display;
    use serde::{Serialize, Serializer, Deserialize, Deserializer};
    use serde_hex::{SerHex, CompactPfx};

    pub fn deserialize<'d, D: Deserializer<'d>, R: FromStr + SerHex<CompactPfx>>(d: D) -> Result<Option<R>, D::Error> where <R as FromStr>::Err: Display {
        use serde::de::Error;
        String::deserialize(d).and_then(|res| match res.is_empty() {
            true => Ok(None),
            false if res.starts_with("0x") =>
                SerHex::from_hex(&res).map(Some)
                    .map_err(D::Error::custom),
            false => FromStr::from_str(&res).map(Some)
                .map_err(D::Error::custom),
        })
    }

    pub fn serialize<S: Serializer, R: SerHex<CompactPfx>>(this: &Option<R>, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::Error;
        match this {
            Some(this) => this.into_hex()
                .map_err(S::Error::custom)
                .and_then(|v| v.serialize(s)),
            None => "".serialize(s),
        }
    }
}

mod hex_serde {
    use std::str::FromStr;
    use std::fmt::Display;
    use serde::{Serializer, Deserialize, Deserializer};
    use serde_hex::{SerHex, CompactPfx};

    pub fn deserialize<'d, D: Deserializer<'d>, R: FromStr + SerHex<CompactPfx>>(d: D) -> Result<R, D::Error> where <R as FromStr>::Err: Display {
        use serde::de::Error;
        String::deserialize(d).and_then(|res| match res.starts_with("0x") {
            true => SerHex::from_hex(&res)
                .map_err(D::Error::custom),
            false => FromStr::from_str(&res)
                .map_err(D::Error::custom),
        })
    }

    pub fn serialize<S: Serializer, R: SerHex<CompactPfx>>(this: &R, s: S) -> Result<S::Ok, S::Error> {
        this.serialize(s)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Keymap {
    #[serde(rename = "Linux Name")]
    linux_name: Option<String>,
    #[serde(rename = "Linux Keycode", with = "hex_serde")]
    linux_keycode: u16,
    #[serde(rename = "OS-X Name")]
    mac_name: Option<String>,
    #[serde(rename = "OS-X Keycode", with = "hex_opt_serde")]
    mac_keycode: Option<u16>,
    #[serde(rename = "AT set1 keycode", with = "hex_opt_serde")]
    at_set1_keycode: Option<u16>,
    #[serde(rename = "AT set2 keycode", with = "hex_opt_serde")]
    at_set2_keycode: Option<u16>,
    #[serde(rename = "AT set3 keycode", with = "hex_opt_serde")]
    at_set3_keycode: Option<u16>,
    #[serde(rename = "USB Keycodes", with = "hex_opt_serde")]
    usb_keycodes: Option<u16>,
    #[serde(rename = "Win32 Name")]
    win32_name: Option<String>,
    #[serde(rename = "Win32 Keycode", with = "hex_opt_serde")]
    win32_keycode: Option<u16>,
    #[serde(rename = "Xwin XT")]
    xwin_xt: Option<String>,
    #[serde(rename = "Xfree86 KBD XT")]
    xfree86_kbd_xt: Option<String>,
    #[serde(rename = "X11 keysym name")]
    x11_keysym_name: Option<String>,
    #[serde(rename = "X11 keysym", with = "hex_opt_serde")]
    x11_keysym: Option<u16>,
    #[serde(rename = "HTML code")]
    html_code: Option<String>,
    #[serde(rename = "XKB key name")]
    xkb_key_name: Option<String>,
    #[serde(rename = "QEMU QKeyCode")]
    qemu_qkeycode: Option<String>,
    #[serde(rename = "Sun KBD", with = "hex_opt_serde")]
    sub_kbd: Option<u16>,
    #[serde(rename = "Apple ADB", with = "hex_opt_serde")]
    apple_adb: Option<u16>,
}

impl Keymap {
    pub fn qemu_qkeycode(&self) -> Option<QKeyCode> {
        self.qemu_qkeycode.as_ref()
            .map(|qkeycode| QKeyCode::from_name(&qkeycode.to_ascii_lowercase()).unwrap())
    }

    /// xtkbd + special re-encoding of high bit
    pub fn qemu_qnum(&self) -> u8 {
        let at1 = self.at_set1_keycode.unwrap_or_default();
        at1 as u8 | if at1 > 0x7f {
            0x80
        } else {
            0
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Keymaps {
    keymaps: Vec<Keymap>,
}

impl Keymaps {
    pub fn from_csv() -> Self {
        let csv_data = include_bytes!("../keymaps.csv");
        let keymaps = csv::Reader::from_reader(&mut &csv_data[..]).deserialize()
            .collect::<Result<Vec<_>, _>>().unwrap();
        Self {
            keymaps,
        }
    }

    pub fn qkeycode_keycodes(&self) -> Box<[QKeyCode]> {
        let max = self.keymaps.iter().map(|k| k.linux_keycode).max().unwrap_or_default();
        (0..=max).map(|i| self.keymaps.iter().find(|k| k.linux_keycode == i)
            .and_then(|k| k.qemu_qkeycode())
            .unwrap_or(QKeyCode::unmapped)
        ).collect()
    }

    pub fn qnum_keycodes(&self) -> Box<[u8]> {
        let max = self.keymaps.iter().map(|k| k.linux_keycode).max().unwrap_or_default();
        (0..=max).map(|i| self.keymaps.iter().find(|k| k.linux_keycode == i)
            .map(|k| k.qemu_qnum())
            .unwrap_or(0)
        ).collect()
    }
}

#[test]
fn keymaps_csv() {
    let keymaps = Keymaps::from_csv();
    println!("{:#?}", keymaps);
    println!("{:#?}", keymaps.qkeycode_keycodes());
}
