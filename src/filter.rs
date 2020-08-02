use std::sync::atomic::{AtomicU8, Ordering};
use enumflags2::BitFlags;
use config::ConfigInputEvent;
use input::InputEvent;

pub struct InputEventFilter {
    filter: AtomicU8,
}

impl InputEventFilter {
    pub fn new<I: IntoIterator<Item=ConfigInputEvent>>(filter: I) -> Self {
        let filter: BitFlags<_> = filter.into_iter().collect();
        InputEventFilter {
            filter: AtomicU8::new(filter.bits()),
        }
    }

    pub fn empty() -> Self {
        InputEventFilter {
            filter: Default::default(),
        }
    }

    pub fn filter(&self) -> BitFlags<ConfigInputEvent> {
        unsafe {
            BitFlags::new(self.filter.load(Ordering::Relaxed))
        }
    }

    pub fn filter_set(&self, value: BitFlags<ConfigInputEvent>) {
        self.filter.store(value.bits(), Ordering::Relaxed)
    }

    pub fn filter_modify<F: FnOnce(&mut BitFlags<ConfigInputEvent>)>(&self, f: F) {
        // TODO: inconsistent
        let mut filter = self.filter();
        f(&mut filter);
        self.filter_set(filter);
    }

    pub fn filter_event(&self, e: &InputEvent) -> bool {
        if let Some(flags) = ConfigInputEvent::from_event(e).map(BitFlags::from) {
            !self.filter().contains(flags)
        } else {
            // just allow sync (and other unknown?) events through
            true
        }
    }

    pub fn set_filter<I: IntoIterator<Item=ConfigInputEvent>>(&self, filter: I) {
        self.filter_modify(|f| f.insert(filter.into_iter().collect::<BitFlags<_>>()))
    }

    pub fn unset_filter<I: IntoIterator<Item=ConfigInputEvent>>(&self, filter: I) {
        self.filter_modify(|f| f.remove(filter.into_iter().collect::<BitFlags<_>>()))
    }
}
