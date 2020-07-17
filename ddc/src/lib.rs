use std::fmt;
use failure::{Error, format_err};

#[cfg(feature = "ddcutil")]
pub mod ddcutil;

#[cfg(feature = "ddc-hi")]
pub mod ddc;

pub const FEATURE_CODE_INPUT: u8 = 0x60;

#[derive(failure_derive::Fail, Debug)]
pub enum DdcError {
    #[fail(display = "Display not found")]
    DisplayNotFound,
    #[fail(display = "Feature code not found")]
    FeatureCodeNotFound,
}

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

    fn inputs(&mut self) -> Result<Vec<u8>, Self::Error>;

    fn get_input(&mut self) -> Result<u8, Self::Error>;
    fn set_input(&mut self, value: u8) -> Result<(), Self::Error>;

    fn find_guest_input(&mut self, our_input: u8) -> Result<Option<u8>, Self::Error> {
        self.inputs()
            .map(|inputs| find_guest_input(&inputs, our_input))
    }
}

pub fn find_guest_input(inputs: &[u8], our_input: u8) -> Option<u8> {
    inputs.iter().copied()
        .filter(|&value| value != our_input)
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

    fn inputs(&mut self) -> Result<Vec<u8>, Self::Error> {
        Err(Self::error())
    }

    fn get_input(&mut self) -> Result<u8, Self::Error> {
        Err(Self::error())
    }

    fn set_input(&mut self, _value: u8) -> Result<(), Self::Error> {
        Err(Self::error())
    }

    fn find_guest_input(&mut self, _our_input: u8) -> Result<Option<u8>, Self::Error> {
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
