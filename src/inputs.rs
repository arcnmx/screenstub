use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::cell::Cell;
use std::rc::Rc;
use futures_cpupool::{self, CpuPool};
use futures::sync::oneshot;
use futures::{future, Future};
use tokio_core::reactor::Handle;
use failure::Error;
use qemu::Qemu;
use config::{ConfigInput, ConfigMonitor, ConfigDdcHost, ConfigDdcGuest};
use exec::exec;
use ddc::{Monitor, SearchDisplay, SearchInput};

pub struct Inputs {
    pool: CpuPool,
    handle: Handle,
    qemu: Rc<Qemu>,
    input_guest: Arc<SearchInput>,
    input_host: Arc<SearchInput>,
    input_host_value: Arc<AtomicUsize>,
    showing_guest: Rc<Cell<bool>>,
    host: ConfigDdcHost,
    guest: ConfigDdcGuest,
    #[cfg(feature = "with-ddcutil")]
    ddc: Arc<Mutex<Monitor>>,
}

fn convert_display(monitor: ConfigMonitor) -> SearchDisplay {
    SearchDisplay {
        manufacturer_id: monitor.manufacturer,
        model_name: monitor.model,
        serial_number: monitor.serial,
        path: None, // TODO: i2c bus selection
    }
}

fn convert_input(input: ConfigInput) -> SearchInput {
    SearchInput {
        value: input.value,
        name: input.name,
    }
}

const INVALID_INPUT: usize = 0x100;

impl Inputs {
    pub fn new(handle: Handle, qemu: Rc<Qemu>, display: ConfigMonitor, input_host: ConfigInput, input_guest: ConfigInput, host: ConfigDdcHost, guest: ConfigDdcGuest) -> Self {
        Inputs {
            pool: futures_cpupool::Builder::new()
                .pool_size(1)
                .name_prefix("DDC")
                .create(),
            handle: handle,
            qemu: qemu,
            input_guest: Arc::new(convert_input(input_guest)),
            input_host: Arc::new(convert_input(input_host)),
            input_host_value: Arc::new(AtomicUsize::new(INVALID_INPUT)),
            showing_guest: Rc::new(Cell::new(false)), // TODO: what if we start when it is showing?? use guest-exec to check if monitor is reachable?
            host: host,
            guest: guest,
            #[cfg(feature = "with-ddcutil")]
            ddc: Arc::new(Mutex::new(Monitor::new(convert_display(display)))),
        }
    }

    pub fn detect_guest(&self) -> Box<Future<Item=(), Error=Error>> {
        Box::new(self.qemu.connect_qga(&self.handle).map(drop).map_err(Error::from)) as Box<_>
    }

    fn map_input_arg<S: AsRef<str>>(s: &S, input: Option<u8>) -> String {
        let s = s.as_ref();
        if let Some(input) = input {
            if s == "{}" {
                format!("{}", input)
            } else if s == "{:x}" {
                format!("{:02x}", input)
            } else if s == "0x{:x}" {
                format!("0x{:02x}", input)
            } else {
                s.to_owned()
            }
        } else {
            s.to_owned()
        }
    }

    pub fn showing_guest(&self) -> bool {
        self.showing_guest.get()
    }

    pub fn show_guest(&self) -> Box<Future<Item=(), Error=Error>> {
        let showing_guest = self.showing_guest.clone();
        match self.host {
            ConfigDdcHost::None => Box::new(future::ok(())) as Box<_>,
            #[cfg(feature = "with-ddcutil")]
            ConfigDdcHost::Libddcutil => {
                let ddc = self.ddc.clone();
                let input = self.input_guest.clone();
                let input_host = self.input_host.clone();
                let input_host_value = self.input_host_value.clone();
                let pool = self.pool.clone();
                Box::new(self.detect_guest().and_then(move |_| oneshot::spawn_fn(move || {
                    let mut ddc = ddc.lock().map_err(|e| format_err!("DDC mutex poisoned {:?}", e))?;
                    ddc.to_display()?;
                    if let Some(input) = ddc.our_input() {
                        if input_host.name.is_some() {
                            if let Some(input) = ddc.match_input(&input_host) {
                                input_host_value.store(input as _, Ordering::Relaxed);
                            }
                        } else {
                            input_host_value.store(input as _, Ordering::Relaxed);
                        }
                    }
                    if let Some(input) = ddc.match_input(&input) {
                        ddc.set_input(input)
                    } else {
                        Err(format_err!("DDC guest input source not found"))
                    }
                }, &pool))
                .inspect(move |&()| showing_guest.set(true))) as Box<_>
            },
            ConfigDdcHost::Ddcutil => {
                Box::new(future::err(format_err!("ddcutil unimplemented"))) as Box<_>
            },
            ConfigDdcHost::Exec(ref args) => {
                let input = self.input_guest.value;
                Box::new(exec(&self.handle, args.into_iter().map(|i| Self::map_input_arg(i, input)))
                    .inspect(move |_| showing_guest.set(true))
                ) as Box<_>
            },
        }
    }

    pub fn show_host(&self) -> Box<Future<Item=(), Error=Error>> {
        let input_host_value = self.input_host_value.load(Ordering::Relaxed);
        let input = self.input_host.value.or_else(|| if input_host_value != INVALID_INPUT { Some(input_host_value as u8) } else { None });
        let showing_guest = self.showing_guest.clone();

        match self.guest {
            ConfigDdcGuest::None => {
                self.showing_guest.set(false);
                Box::new(future::ok(())) as Box<_>
            },
            ConfigDdcGuest::Exec(ref args) => {
                let input = self.input_guest.value;
                Box::new(exec(&self.handle, args.into_iter().map(|i| Self::map_input_arg(i, input)))
                    .inspect(move |&()| showing_guest.set(false))
                ) as Box<_>
            },
            ConfigDdcGuest::GuestExec(ref args) => {
                Box::new(
                    self.qemu.guest_exec(&self.handle, args.into_iter().map(|i| Self::map_input_arg(i, input)))
                    .map(drop)
                    .inspect(move |&()| showing_guest.set(false))
                ) as Box<_>
            },
        }
    }
}
