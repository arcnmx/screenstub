use std::fmt;
use ddc_hi::{Display, Query, Ddc};
use crate::{SearchDisplay, DdcMonitor, FEATURE_CODE_INPUT};
use anyhow::Error;

pub struct Monitor {
    display: Display,
    sources: Vec<u8>,
}

impl From<Display> for Monitor {
    fn from(display: Display) -> Self {
        Self {
            display,
            sources: Default::default(),
        }
    }
}

fn query(search: &SearchDisplay) -> Query {
    let mut query = Query::Any;

    if let Some(id) = search.backend_id.as_ref() {
        query = Query::And(vec![
            query,
            Query::Id(id.into()),
        ]);
    }

    if let Some(manufacturer) = search.manufacturer_id.as_ref() {
        query = Query::And(vec![
            query,
            Query::ManufacturerId(manufacturer.into()),
        ]);
    }

    if let Some(model) = search.model_name.as_ref() {
        query = Query::And(vec![
            query,
            Query::ModelName(model.into()),
        ]);
    }

    if let Some(serial) = search.serial_number.as_ref() {
        query = Query::And(vec![
            query,
            Query::SerialNumber(serial.into()),
        ]);
    }

    query
}

impl DdcMonitor for Monitor {
    type Error = Error;

    fn enumerate() -> Result<Vec<Self>, Self::Error> where Self: Sized {
        Ok(Display::enumerate().into_iter().map(From::from).collect())
    }

    fn matches(&self, search: &SearchDisplay) -> bool {
        let query = query(search);
        query.matches(&self.display.info)
    }

    fn sources(&mut self) -> Result<Vec<u8>, Self::Error> {
        if self.sources.is_empty() {
            let caps = self.display.handle.capabilities()?;
            self.sources = caps.vcp_features
                .get(&FEATURE_CODE_INPUT)
                .map(|desc| desc.values.iter().map(|(&value, _)| value).collect())
                .unwrap_or_default();
        }
        Ok(self.sources.clone())
    }

    fn get_source(&mut self) -> Result<u8, Self::Error> {
        self.display.handle.get_vcp_feature(FEATURE_CODE_INPUT)
            .map(|v| v.value() as u8)
    }

    fn set_source(&mut self, value: u8) -> Result<(), Self::Error> {
        self.display.handle.set_vcp_feature(FEATURE_CODE_INPUT, value as u16)
    }
}

impl fmt::Display for Monitor {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        writeln!(f, "ID: {}", self.display.info.id)?;
        if let Some(mf) = self.display.info.manufacturer_id.as_ref() {
            writeln!(f, "Manufacturer: {}", mf)?
        }
        if let Some(model) = self.display.info.model_name.as_ref() {
            writeln!(f, "Model: {}", model)?
        }
        if let Some(serial) = self.display.info.serial_number.as_ref() {
            writeln!(f, "Serial: {}", serial)?
        }

        Ok(())
    }
}
