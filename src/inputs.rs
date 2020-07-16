use std::future::Future;
use std::sync::atomic::{AtomicUsize, AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use failure::{Error, format_err};
use qemu::Qemu;
use config::{ConfigInput, ConfigMonitor, ConfigDdcHost, ConfigDdcGuest};
use crate::exec::exec;
use ddc::{SearchDisplay, SearchInput};
#[cfg(feature = "with-ddcutil")]
use ddc::ddcutil::Monitor;

pub struct Inputs {
    qemu: Arc<Qemu>,
    input_guest: Arc<SearchInput>,
    input_host: Arc<SearchInput>,
    input_host_value: Arc<AtomicUsize>,
    showing_guest: Arc<AtomicBool>,
    host: Arc<ConfigDdcHost>,
    guest: Arc<ConfigDdcGuest>,
    #[cfg(feature = "with-ddcutil")]
    ddc: Arc<Mutex<Monitor>>,
}

#[cfg(feature = "with-ddcutil")]
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
    pub fn new(qemu: Arc<Qemu>, display: ConfigMonitor, input_host: ConfigInput, input_guest: ConfigInput, host: ConfigDdcHost, guest: ConfigDdcGuest) -> Self {
        Inputs {
            qemu,
            input_guest: Arc::new(convert_input(input_guest)),
            input_host: Arc::new(convert_input(input_host)),
            input_host_value: Arc::new(AtomicUsize::new(INVALID_INPUT)),
            showing_guest: Arc::new(AtomicBool::new(false)), // TODO: what if we start when it is showing?? use guest-exec to check if monitor is reachable?
            host: Arc::new(host),
            guest: Arc::new(guest),
            #[cfg(feature = "with-ddcutil")]
            ddc: Arc::new(Mutex::new(Monitor::new(convert_display(display)))),
        }
    }

    async fn detect_guest_(qemu: &Qemu) -> Result<(), Error> {
        qemu.connect_qga().await
            .map(drop).map_err(Error::from)
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
        self.showing_guest.load(Ordering::Relaxed)
    }

    pub fn show_guest(&self) -> impl Future<Output=Result<(), Error>> {
        let showing_guest = self.showing_guest.clone();
        let input = self.input_guest.clone();
        let host = self.host.clone();
        #[cfg(feature = "with-ddcutil")]
        let (ddc, input_host, input_host_value, qemu) = (
            self.ddc.clone(),
            self.input_host.clone(),
            self.input_host_value.clone(),
            self.qemu.clone(),
        );
        async move { match &*host {
            ConfigDdcHost::None => Ok(()),
            #[cfg(feature = "with-ddcutil")]
            ConfigDdcHost::Libddcutil => {
                Self::detect_guest_(&qemu).await?;
                showing_guest.store(true, Ordering::Relaxed);
                tokio::task::spawn_blocking(move || {
                    let mut ddc = ddc.lock().unwrap();
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
                }).await
                    .map_err(From::from).and_then(|r| r)
            },
            #[cfg(feature = "with-ddc")]
            ConfigDdcHost::Ddc => {
                Err(format_err!("ddc unimplemented"))
            },
            ConfigDdcHost::Ddcutil => {
                Err(format_err!("ddcutil unimplemented"))
            },
            ConfigDdcHost::Exec(ref args) => {
                let res = exec(args.into_iter().map(|i| Self::map_input_arg(i, input.value))).into_future().await;
                showing_guest.store(true, Ordering::Relaxed);
                res
            },
        } }
    }

    pub fn show_host(&self) -> impl Future<Output=Result<(), Error>> + Send + 'static {
        let input_host_value = self.input_host_value.load(Ordering::Relaxed);
        let input = self.input_host.value.or_else(|| if input_host_value != INVALID_INPUT { Some(input_host_value as u8) } else { None });
        let showing_guest = self.showing_guest.clone();
        let input_guest = self.input_guest.clone();
        let qemu = self.qemu.clone();
        let guest = self.guest.clone();

        async move {
            match &*guest {
                ConfigDdcGuest::None => {
                    showing_guest.store(false, Ordering::Relaxed);
                    Ok(())
                },
                ConfigDdcGuest::Exec(args) => {
                    let input = input_guest.value;
                    let res = exec(args.iter().map(|i| Self::map_input_arg(i, input))).into_future().await;
                    showing_guest.store(false, Ordering::Relaxed);
                    res
                },
                ConfigDdcGuest::GuestExec(args) => {
                    let res = qemu.guest_exec(args.iter().map(|i| Self::map_input_arg(i, input))).into_future().await;
                    showing_guest.store(false, Ordering::Relaxed);
                    res.map(drop)
                },
            }
        }
    }
}
