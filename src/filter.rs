use std::collections::HashSet;
use config::ConfigInputEvent;
use input::InputEvent;

pub struct InputEventFilter {
    filter: HashSet<ConfigInputEvent>,
}

impl InputEventFilter {
    pub fn new<I: IntoIterator<Item=ConfigInputEvent>>(filter: I) -> Self {
        InputEventFilter {
            filter: filter.into_iter().collect(),
        }
    }

    pub fn empty() -> Self {
        InputEventFilter {
            filter: Default::default(),
        }
    }

    pub fn filter_event(&self, e: &InputEvent) -> bool {
        ConfigInputEvent::from_event(e).map(|e| !self.filter.contains(&e)).unwrap_or(true)
    }

    pub fn set_filter<I: IntoIterator<Item=ConfigInputEvent>>(&mut self, filter: I) {
        filter.into_iter().for_each(|f| { self.filter.insert(f); })
    }

    pub fn unset_filter<I: IntoIterator<Item=ConfigInputEvent>>(&mut self, filter: I) {
        filter.into_iter().for_each(|f| { self.filter.remove(&f); })
    }
}
