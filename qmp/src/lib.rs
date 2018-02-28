#[macro_use]
extern crate log;
extern crate serde;
#[macro_use]
extern crate serde_derive;
extern crate serde_json;
extern crate base64 as b64;

use serde::de::DeserializeOwned;
use serde::Serialize;

mod base64 {
    use serde::{Serialize, Serializer, Deserialize, Deserializer};
    use serde::de::{Error, Unexpected};
    use b64::{self, DecodeError};

    pub fn serialize<S: Serializer>(data: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        b64::encode(data).serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        // TODO: deserialize to borrowed &str
        let str = String::deserialize(deserializer)?;

        b64::decode(&str)
            .map_err(|e| de_err(&str, e))
    }

    pub fn de_err<E: Error>(str: &str, err: DecodeError) -> E {
        match err {
            DecodeError::InvalidByte(..) =>
                E::invalid_value(Unexpected::Str(str), &"base64"),
            DecodeError::InvalidLength =>
                E::invalid_length(str.len(), &"valid base64 length"),
        }
    }
}

mod base64_opt {
    use serde::{Serializer, Deserialize, Deserializer};
    use {b64, base64};

    pub fn serialize<S: Serializer>(data: &Option<Vec<u8>>, serializer: S) -> Result<S::Ok, S::Error> {
        base64::serialize(data.as_ref().expect("use skip_serializing_with"), serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<Vec<u8>>, D::Error> {
        // TODO: deserialize to borrowed &str
        let str = <Option<String>>::deserialize(deserializer)?;
        if let Some(str) = str {
            b64::decode(&str)
                .map(Some)
                .map_err(|e| base64::de_err(&str, e))
        } else {
            Ok(None)
        }
    }
}

// TODO: differentiate QGA and QMP commands
// TODO: rename to qapi
// TODO: autogenerate these structures from json schemas
// - https://github.com/qemu/qemu/blob/master/qapi-schema.json
// - https://github.com/qemu/qemu/tree/master/qapi *.json

pub trait QapiCommand: Serialize {
    type Ok: DeserializeOwned;

    const NAME: &'static str;
}

pub trait Qapi {
    type Error: From<QapiError>;

    fn handshake() -> Result<(), Self::Error>;
    fn execute<C: QapiCommand>(&mut self, command: C) -> Result<C::Ok, Self::Error>;
}

#[derive(Debug, Clone, Deserialize)]
pub struct QapiError {
    pub desc: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum QapiResponse<C: QapiCommand> {
    Err(QapiError),
    Ok(C::Ok),
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct GuestExec {
    pub path: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub arg: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<String>,
    #[serde(with = "base64_opt")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_data: Option<Vec<u8>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capture_output: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct GuestExecResponse {
    pub pid: i32,
}

impl QapiCommand for GuestExec {
    type Ok = GuestExecResponse;

    const NAME: &'static str = "guest-exec";
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct GuestExecStatus {
    pub pid: i32,
}

impl From<GuestExecResponse> for GuestExecStatus {
    fn from(v: GuestExecResponse) -> Self {
        GuestExecStatus {
            pid: v.pid,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct GuestExecStatusResponse {
    pub exited: bool,
    #[serde(default)]
    pub exitcode: Option<i32>,
    #[serde(default)]
    pub signal: Option<i32>,
    #[serde(default, with = "base64_opt")]
    pub out_data: Option<Vec<u8>>,
    #[serde(default, with = "base64_opt")]
    pub err_data: Option<Vec<u8>>,
    #[serde(default)]
    pub out_truncated: Option<bool>,
    #[serde(default)]
    pub err_truncated: Option<bool>,
}

impl QapiCommand for GuestExecStatus {
    type Ok = GuestExecStatusResponse;

    const NAME: &'static str = "guest-exec-status";
}
