use std::fmt;
use anyhow::{Error, format_err};

#[cfg(feature = "ddcutil")]
pub mod ddcutil;

#[cfg(feature = "ddc-hi")]
pub mod ddc;

pub const FEATURE_CODE_INPUT: u8 = 0x60;

#[derive(Debug)]
pub enum DdcError {
    DisplayNotFound,
    FeatureCodeNotFound,
}

impl fmt::Display for DdcError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let msg = match self {
            DdcError::DisplayNotFound => "Display not found",
            DdcError::FeatureCodeNotFound => "Feature code not found",
        };
        f.write_str(msg)
    }
}

impl std::error::Error for DdcError { }

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SearchDisplay {
    pub backend_id: Option<String>,
    pub manufacturer_id: Option<String>,
    pub model_name: Option<String>,
    pub serial_number: Option<String>,
    #[cfg(feature = "ddcutil")]
    pub path: Option<::ddcutil::DisplayPath>,
    #[cfg(not(feature = "ddcutil"))]
    pub path: Option<()>,
}

pub trait DdcMonitor: fmt::Display {
    type Error: Into<Error>;

    fn search(search: &SearchDisplay) -> Result<Option<Self>, Self::Error> where Self: Sized {
        Self::enumerate().map(|displays|
            displays.into_iter().find(|d| d.matches(search))
        )
    }

    fn matches(&self, search: &SearchDisplay) -> bool;

    fn enumerate() -> Result<Vec<Self>, Self::Error> where Self: Sized;

    fn sources(&mut self) -> Result<Vec<u8>, Self::Error>;

    fn get_source(&mut self) -> Result<u8, Self::Error>;
    fn set_source(&mut self, value: u8) -> Result<(), Self::Error>;

    fn find_guest_source(&mut self, our_source: u8) -> Result<Option<u8>, Self::Error> {
        self.sources()
            .map(|sources| find_guest_source(&sources, our_source))
    }
}

pub fn find_guest_source(sources: &[u8], our_source: u8) -> Option<u8> {
    sources.iter().copied()
        .filter(|&value| value != our_source)
        .next()
}

pub struct DummyMonitor;

impl DummyMonitor {
    fn error() -> Error {
        format_err!("ddc disabled")
    }
}

impl DdcMonitor for DummyMonitor {
    type Error = Error;

    fn search(_search: &SearchDisplay) -> Result<Option<Self>, Self::Error> {
        Err(Self::error())
    }

    fn matches(&self, _search: &SearchDisplay) -> bool {
        false
    }

    fn enumerate() -> Result<Vec<Self>, Self::Error> {
        Err(Self::error())
    }

    fn sources(&mut self) -> Result<Vec<u8>, Self::Error> {
        Err(Self::error())
    }

    fn get_source(&mut self) -> Result<u8, Self::Error> {
        Err(Self::error())
    }

    fn set_source(&mut self, _value: u8) -> Result<(), Self::Error> {
        Err(Self::error())
    }

    fn find_guest_source(&mut self, _our_source: u8) -> Result<Option<u8>, Self::Error> {
        Err(Self::error())
    }
}

impl fmt::Display for DummyMonitor {
    fn fmt(&self, _: &mut fmt::Formatter) -> fmt::Result {
        panic!("{}", Self::error())
    }
}

#[cfg(feature = "ddc-hi")]
pub type Monitor = ddc::Monitor;

#[cfg(all(not(feature = "ddc-hi"), feature = "ddcutil"))]
pub type Monitor = ddcutil::Monitor;

#[cfg(not(any(feature = "ddcutil", feature = "ddc-hi")))]
pub type Monitor = DummyMonitor;
