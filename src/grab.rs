use std::collections::HashMap;
use std::path::Path;
use std::rc::Rc;
use std::io;
use futures::unsync::mpsc as un_mpsc;
use futures::{Sink, Stream, Future};
use failure::Error;
use tokio_core::reactor::Handle;
use tokio_fuse::SharedFuse;
use input::{InputEvent, EvdevHandle, InputId};
use uinput::{UInputSink, Evdev};
use config::ConfigInputEvent;
use filter::InputEventFilter;
use futures_stream_select_all::select_all;

pub enum Grab {
    XCore,
    Evdev(GrabEvdev),
}

impl From<GrabEvdev> for Grab {
    fn from(g: GrabEvdev) -> Self {
        Grab::Evdev(g)
    }
}

pub struct GrabEvdev {
    devices: HashMap<InputId, (SharedFuse<UInputSink>, EvdevHandle)>,
    filter: Rc<InputEventFilter>,
}

impl GrabEvdev {
    pub fn new<P, I, F>(handle: &Handle, devices: I, filter: F) -> Result<Self, Error> where
        P: AsRef<Path>,
        I: IntoIterator<Item=P>,
        F: IntoIterator<Item=ConfigInputEvent>,
    {
        let devices: io::Result<_> = devices.into_iter().map(|dev| -> io::Result<_> {
            let dev = Evdev::open(&dev)?;

            let evdev = dev.evdev();

            let id = evdev.device_id()?;
            let stream = SharedFuse::new(dev.to_sink(handle)?);

            Ok((id, (stream, evdev)))
        }).collect();

        Ok(GrabEvdev {
            devices: devices?,
            filter: Rc::new(InputEventFilter::new(filter)),
        })
    }

    pub fn grab(&self, grab: bool) -> io::Result<()> {
        Ok(for (_, &(_, ref evdev)) in &self.devices {
            evdev.grab(grab)?;
        })
    }

    pub fn spawn<S>(&self, handle: &Handle, sink: S, error_sender: un_mpsc::Sender<Error>) where
        S: Sink<SinkItem=InputEvent> + Clone + 'static,
        Error: From<S::SinkError>,
    {
        let filter = self.filter.clone();
        handle.spawn(
            select_all(
                self.devices.iter().map(|(_, &(ref stream, _))| stream).cloned()
            ).map_err(From::from)
                .filter(move |e| filter.filter_event(e))
                .forward(sink.sink_map_err(Error::from))
                .map(drop).or_else(|e| error_sender.send(e).map(drop).map_err(drop))
        );
    }

    pub fn evdevs(&self) -> Vec<&EvdevHandle> {
        // TODO: come on
        self.devices.iter().map(|(_, &(_, ref evdev))| evdev).collect()
    }
}

impl Drop for GrabEvdev {
    fn drop(&mut self) {
        for (_, (mut stream, _)) in self.devices.drain() {
            stream.fuse_inner();
        }
    }
}
