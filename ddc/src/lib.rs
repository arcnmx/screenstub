#[cfg(feature = "ddcutil")]
pub mod ddcutil;

#[cfg(feature = "ddc")]
pub mod ddc;

#[derive(failure_derive::Fail, Debug)]
pub enum DdcError {
    #[fail(display = "Display not found")]
    DisplayNotFound,
    #[fail(display = "Feature code not found")]
    FeatureCodeNotFound,
}

#[derive(Debug, Clone, Default)]
pub struct SearchDisplay {
    pub manufacturer_id: Option<String>,
    pub model_name: Option<String>,
    pub serial_number: Option<String>,
    #[cfg(feature = "ddcutil")]
    pub path: Option<::ddcutil::DisplayPath>,
    #[cfg(not(feature = "ddcutil"))]
    pub path: Option<()>,
}

#[derive(Debug, Clone, Default)]
pub struct SearchInput {
    pub value: Option<u8>,
    pub name: Option<String>,
}
