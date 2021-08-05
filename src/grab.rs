use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::io;
use futures::channel::mpsc as un_mpsc;
use futures::{Sink, SinkExt, StreamExt, FutureExt, stream, future};
use anyhow::Error;
use input::{InputEvent, InputId};
use uinput::{UInputSink, EvdevHandle, Evdev};
use config::ConfigInputEvent;
use crate::filter::InputEventFilter;

/*pub enum Grab {
    XCore,
    Evdev(GrabEvdev),
}

impl From<GrabEvdev> for Grab {
    fn from(g: GrabEvdev) -> Self {
        Grab::Evdev(g)
    }
}*/

pub struct GrabEvdev {
    devices: HashMap<InputId, UInputSink>,
    filter: Arc<InputEventFilter>,
}

impl GrabEvdev {
    pub fn new<P, I, F>(devices: I, filter: F) -> Result<Self, Error> where
        P: AsRef<Path>,
        I: IntoIterator<Item=P>,
        F: IntoIterator<Item=ConfigInputEvent>,
    {
        let devices: io::Result<_> = devices.into_iter().map(|dev| -> io::Result<_> {
            let dev = Evdev::open(&dev)?;

            let evdev = dev.evdev();

            let id = evdev.device_id()?;
            let stream = dev.to_sink()?;

            Ok((id, stream))
        }).collect();

        Ok(GrabEvdev {
            devices: devices?,
            filter: Arc::new(InputEventFilter::new(filter)),
        })
    }

    pub fn grab(&self, grab: bool) -> io::Result<()> {
        Ok(for (_, ref uinput) in &self.devices {
            if let Some(evdev) = uinput.evdev() {
                evdev.grab(grab)?;
            }
        })
    }

    pub fn spawn<S>(self, mut sink: S, mut error_sender: un_mpsc::Sender<Error>) -> future::AbortHandle where
        S: Sink<InputEvent> + Unpin + Clone + Send + 'static,
        Error: From<S::Error>,
    {
        let fut = async move {
            let mut select = stream::select_all(
                self.devices.into_iter().map(|(_, stream)| stream)
            );
            while let Some(e) = select.next().await {
                let e = e?;
                if self.filter.filter_event(&e) {
                    if sink.send(e).await.is_err() {
                        break
                    }
                }
            }

            Ok(())
        }.then(move |r| async move { match r {
            Err(e) => {
                let _ = error_sender.send(e).await;
            },
            _ => (),
        } });
        let (fut, handle) = future::abortable(fut);
        tokio::spawn(fut);
        handle
    }

    pub fn evdevs(&self) -> Vec<EvdevHandle> {
        // TODO: come on
        self.devices.iter().filter_map(|(_, ref stream)| stream.evdev()).collect()
    }
}

/*impl Drop for GrabEvdev {
    fn drop(&mut self) {
        for (_, mut stream) in self.devices.drain() {
            // TODO: stream.close();
        }
    }
}*/
