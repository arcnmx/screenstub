use std::collections::HashMap;
use std::mem::replace;
use std::fmt;
use failure::Error;
use ddcutil::{DisplayInfo, Display, FeatureInfo};
use crate::{DdcError, DdcMonitor, SearchDisplay, FEATURE_CODE_INPUT};

impl SearchDisplay {
    pub fn matches(&self, info: &DisplayInfo) -> bool {
        let matches = [
            (&info.manufacturer_id(), &self.manufacturer_id),
            (&info.model_name(), &self.model_name),
            (&info.serial_number(), &self.serial_number),
        ].iter().filter_map(|&(i, m)| m.as_ref().map(|m| (i, m)))
            .all(|(i, m)| i == m);

        if let Some(ref path) = self.path {
            matches && path == &info.path()
        } else {
            matches
        }
    }
}

#[derive(Debug)]
pub enum Monitor {
    #[doc(hidden)]
    Search(SearchDisplay),
    #[doc(hidden)]
    Display {
        info: DisplayInfo,
        display: Display,
        input_values: HashMap<u8, String>,
        our_input: Option<u8>,
        search: SearchDisplay,
    },
}

impl Monitor {
    pub fn new(search: SearchDisplay) -> Self {
        Monitor::Search(search)
    }

    pub fn enumerate() -> Result<Vec<Self>, Error> {
        DisplayInfo::enumerate()?.into_iter().map(|i|
            Self::from_display_info(i, None)
        ).collect()
    }

    pub fn search(&self) -> &SearchDisplay {
        match *self {
            Monitor::Search(ref search) => search,
            Monitor::Display { ref search, .. } => search,
        }
    }

    pub fn info(&self) -> Option<&DisplayInfo> {
        match *self {
            Monitor::Search(..) => None,
            Monitor::Display { ref info, .. } => Some(info),
        }
    }

    pub fn inputs(&self) -> Option<&HashMap<u8, String>> {
        match *self {
            Monitor::Search(..) => None,
            Monitor::Display { ref input_values, .. } => Some(input_values),
        }
    }

    pub fn our_input(&self) -> Option<u8> {
        match *self {
            Monitor::Search(..) => None,
            Monitor::Display { our_input, .. } => our_input,
        }
    }

    pub fn other_inputs(&self) -> Vec<(u8, &str)> {
        let ours = self.our_input();
        self.inputs().map(|inputs| inputs.iter().filter(|&(&i, _)| Some(i) != ours)
            .map(|(i, s)| (*i, &s[..])).collect()
        ).unwrap_or(Vec::new())
    }

    pub fn from_display_info(info: DisplayInfo, search: Option<&mut SearchDisplay>) -> Result<Self, Error> {
        let display = info.open()?;
        let caps = display.capabilities()?;
        let input_caps = caps.features.get(&FEATURE_CODE_INPUT).ok_or(DdcError::FeatureCodeNotFound)?;
        let mut input_info = FeatureInfo::from_code(FEATURE_CODE_INPUT, caps.version)?;
        let input_values = input_caps.into_iter().map(|&val|
            (val, input_info.value_names.remove(&val).unwrap_or_else(|| "Unknown".into()))
        ).collect();
        let our_input = display.vcp_get_value(FEATURE_CODE_INPUT).ok().map(|v| v.value() as u8);

        let search = if let Some(search) = search {
            replace(search, Default::default())
        } else {
            Default::default()
        };
        Ok(Monitor::Display {
            info,
            display,
            input_values,
            our_input,
            search,
        })
    }

    fn find_display(&mut self) -> Result<Option<Self>, Error> {
        match *self {
            Monitor::Search(ref mut search) => {
                let displays = DisplayInfo::enumerate()?;
                if let Some(info) = displays.into_iter().find(|d| search.matches(d)) {
                    Self::from_display_info(info, Some(search)).map(Some)
                } else {
                    Err(DdcError::DisplayNotFound.into())
                }
            },
            Monitor::Display { .. } => Ok(None),
        }
    }

    pub fn to_display(&mut self) -> Result<(), Error> {
        if let Some(monitor) = self.find_display()? {
            *self = monitor
        }

        Ok(())
    }

    pub fn reset_handle(&mut self) {
        let search = match *self {
            Monitor::Search(..) => return,
            Monitor::Display { ref mut search, .. } => replace(search, Default::default()),
        };

        *self = Monitor::Search(search);
    }

    pub fn display(&mut self) -> Result<&Display, Error> {
        self.to_display()?;

        match *self {
            Monitor::Search(..) => Err(DdcError::DisplayNotFound.into()),
            Monitor::Display { ref display, .. } => Ok(display),
        }
    }
}

impl DdcMonitor for Monitor {
    type Error = Error;

    fn search(search: &SearchDisplay) -> Result<Option<Self>, Self::Error> where Self: Sized {
        let mut res = Monitor::Search(search.clone());
        res.to_display()?; // TODO: can't distinguish not found vs connection error
        Ok(Some(res))
    }

    fn matches(&self, search: &SearchDisplay) -> bool {
        match self {
            Monitor::Search(s) => s == search,
            Monitor::Display { info, .. } => search.matches(info),
        }
    }

    fn enumerate() -> Result<Vec<Self>, Self::Error> where Self: Sized {
        DisplayInfo::enumerate()?.into_iter().map(|i|
            Self::from_display_info(i, None)
        ).collect()
    }

    fn inputs(&mut self) -> Result<Vec<u8>, Self::Error> {
        self.to_display()?;
        match self {
            Monitor::Search(..) => Ok(Default::default()),
            Monitor::Display { input_values, .. } => Ok(input_values
                .iter().map(|(&value, _)| value).collect()
            ),
        }
    }

    fn get_input(&mut self) -> Result<u8, Error> {
        Ok(self.display()?.vcp_get_value(FEATURE_CODE_INPUT)?.value() as u8)
    }

    fn set_input(&mut self, value: u8) -> Result<(), Self::Error> {
        self.display()?.vcp_set_simple(FEATURE_CODE_INPUT, value).map_err(From::from)
    }
}

impl Default for Monitor {
    fn default() -> Self {
        Self::new(Default::default())
    }
}

impl fmt::Display for Monitor {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let info = match self {
            Monitor::Display { info, .. } => info,
            _ => return Err(fmt::Error),
        };
        writeln!(f, "Manufacturer: {}\nModel: {}\nSerial: {}",
            info.manufacturer_id(), info.model_name(), info.serial_number()
        )
    }
}
