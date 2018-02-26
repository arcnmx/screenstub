use std::collections::HashMap;
use std::mem::replace;
use ddcutil::{DisplayInfo, DisplayPath, Display, FeatureInfo, FeatureCode};
use Error;

const FEATURE_CODE_INPUT: FeatureCode = 0x60;

#[derive(Debug, Clone, Default)]
pub struct Search {
	pub manufacturer_id: Option<String>,
	pub model_name: Option<String>,
	pub serial_number: Option<String>,
	pub path: Option<DisplayPath>,
}

#[derive(Debug, Clone, Default)]
pub struct SearchInput {
	pub value: Option<u8>,
	pub name: Option<String>,
}

impl Search {
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
	Search(Search),
	#[doc(hidden)]
	Display {
		info: DisplayInfo,
		display: Display,
		input_values: HashMap<u8, String>,
		our_input: Option<u8>,
		search: Search,
	},
}

impl Monitor {
	pub fn new(search: Search) -> Self {
		Monitor::Search(search)
	}

	pub fn search(&self) -> &Search {
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

	pub fn match_input(&self, search: &SearchInput) -> Option<u8> {
		self.other_inputs().into_iter().find(|&(other_v, other_name)| {
			if let Some(v) = search.value {
				if other_v != v {
					return false
				}
			}

			if let Some(ref name) = search.name {
				if other_name != name {
					return false
				}
			}

			true
		}).map(|(v, _)| v)
	}

	fn find_display(&mut self) -> Result<Option<Monitor>, Error> {
		match *self {
			Monitor::Search(ref mut search) => {
				let displays = DisplayInfo::enumerate()?;
				if let Some(info) = displays.into_iter().find(|d| search.matches(d)) {
					let display = info.open()?;
					let caps = display.capabilities()?;
					let (input_caps, mut input_info) = caps.features.get(&FEATURE_CODE_INPUT).ok_or(Error::DdcNotFound).and_then(|c|
						FeatureInfo::from_code(FEATURE_CODE_INPUT, caps.version)
							.map(|i| (c, i)).map_err(From::from)
					)?;
					let input_values = input_caps.into_iter().map(|&val|
						(val, input_info.value_names.remove(&val).unwrap_or_else(|| "Unknown".into()))
					).collect();
					let our_input = display.vcp_get_value(FEATURE_CODE_INPUT).ok().map(|v| v.value() as u8);

					let mut search = replace(search, Default::default());
					Ok(Some(Monitor::Display {
						info: info,
						display: display,
						input_values: input_values,
						our_input: our_input,
						search: search,
					}))
				} else {
					Err(Error::DdcNotFound)
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
			Monitor::Search(..) => Err(Error::DdcNotFound),
			Monitor::Display { ref display, .. } => Ok(display),
		}
	}

	pub fn get_input(&mut self) -> Result<(u8, String), Error> {
		let value = self.display()?.vcp_get_value(FEATURE_CODE_INPUT)?.value() as u8;
		Ok((value, self.inputs().unwrap().get(&value).cloned().unwrap_or_else(|| "Unknown".into())))
	}

	pub fn set_input(&mut self, value: u8) -> Result<(), Error> {
		self.display()?.vcp_set_simple(FEATURE_CODE_INPUT, value).map_err(From::from)
	}
}

impl Default for Monitor {
	fn default() -> Self {
		Self::new(Default::default())
	}
}
