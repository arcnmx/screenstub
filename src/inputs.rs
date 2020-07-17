use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use failure::{Error, format_err};
use qemu::Qemu;
use config::{ConfigInput, ConfigMonitor, ConfigDdcMethod};
use crate::exec::exec;
use ddc::{SearchDisplay, DdcMonitor};

type DynMonitor = dyn DdcMonitor<Error=Error> + Send;

pub struct Inputs {
    qemu: Arc<Qemu>,
    input_guest: Option<u8>,
    input_host: Option<u8>,
    showing_guest: Arc<AtomicBool>,
    host: Vec<Arc<ConfigDdcMethod>>,
    guest: Vec<Arc<ConfigDdcMethod>>,
    monitor: Arc<SearchDisplay>,
    ddc: Arc<Mutex<Option<Box<DynMonitor>>>>,
}

fn convert_display(monitor: ConfigMonitor) -> SearchDisplay {
    SearchDisplay {
        backend_id: monitor.id,
        manufacturer_id: monitor.manufacturer,
        model_name: monitor.model,
        serial_number: monitor.serial,
        path: None, // TODO: i2c bus selection?
    }
}

impl Inputs {
    pub fn new(qemu: Arc<Qemu>, display: ConfigMonitor, input_host: ConfigInput, input_guest: ConfigInput, host: Vec<ConfigDdcMethod>, guest: Vec<ConfigDdcMethod>) -> Self {
        Inputs {
            qemu,
            input_guest: input_guest.value(),
            input_host: input_host.value(),
            showing_guest: Arc::new(AtomicBool::new(false)), // TODO: what if we start when it is showing?? use guest-exec to check if monitor is reachable?
            host: host.into_iter().map(Arc::new).collect(),
            guest: guest.into_iter().map(Arc::new).collect(),
            monitor: Arc::new(convert_display(display)),
            ddc: Arc::new(Mutex::new(None)),
        }
    }

    pub async fn fill(&mut self) -> Result<(), Error> {
        tokio::task::block_in_place(move || {
            let mut ddc = self.ddc.lock().unwrap();
            for method in &self.host {
                if self.input_host.is_some() && self.input_guest.is_some() {
                    break
                }
                let ddc = Self::ddc_connect(&mut ddc, method, &self.monitor)?;
                let input_host = match self.input_host {
                    Some(input) => input,
                    None => {
                        let input = ddc.get_input()?;
                        self.input_host = Some(input);
                        input
                    },
                };
                match &self.input_guest {
                    Some(..) => (),
                    None =>
                        self.input_guest = ddc.find_guest_input(input_host)?,
                }
            }

            Ok(())
        })
    }

    async fn detect_guest_(qemu: &Qemu) -> Result<(), Error> {
        qemu.connect_qga().await
            .map(drop).map_err(Error::from)
    }

    fn map_input_arg<S: AsRef<str>>(s: S, input: Option<u8>, host: bool) -> Result<String, Error> {
        let input = input
            .ok_or_else(|| format_err!("DDC {} input source not found",
                if host { "host" } else { "guest" }
            ));
        let s = s.as_ref();
        Ok(if s == "{}" {
            format!("{}", input?)
        } else if s == "{:x}" {
            format!("{:02x}", input?)
        } else if s == "0x{:x}" {
            format!("0x{:02x}", input?)
        } else {
            s.to_owned()
        })
    }

    pub fn showing_guest(&self) -> bool {
        self.showing_guest.load(Ordering::Relaxed)
    }

    pub fn show_guest(&self) -> impl Future<Output=Result<(), Error>> {
        self.show(false)
    }

    pub fn show_host(&self) -> impl Future<Output=Result<(), Error>> {
        self.show(true)
    }

    fn ddc_connect<'a>(ddc: &'a mut Option<Box<DynMonitor>>, method: &ConfigDdcMethod, monitor: &SearchDisplay) -> Result<&'a mut Box<DynMonitor>, Error> {
        if ddc.is_some() {
            // mean workaround for lifetime issues
            match ddc.as_mut() {
                Some(ddc) => Ok(ddc),
                None => unsafe { core::hint::unreachable_unchecked() },
            }
        } else {
            let res = match method {
                #[cfg(feature = "with-ddcutil")]
                ConfigDdcMethod::Libddcutil =>
                    ddc::ddcutil::Monitor::search(&monitor)
                        .map(|r| r.map(|r| Box::new(r)))?,
                #[cfg(not(feature = "with-ddcutil"))]
                ConfigDdcMethod::Libddcutil =>
                    return Err(format_err!("Not compiled for libddcutil")),
                ConfigDdcMethod::Ddcutil =>
                    return Err(format_err!("ddcutil CLI support unimplemented")),
                _ =>
                    ddc::Monitor::search(&monitor)
                        .map(|r| r.map(|r| Box::new(r)))?,
            };
            match res {
                Some(res) =>
                    Ok(ddc.get_or_insert(res)),
                None =>
                    Err(format_err!("DDC monitor not found")),
            }
        }
    }

    pub fn show(&self, host: bool) -> impl Future<Output=Result<(), Error>> {
        let methods: Vec<_> = if self.showing_guest() != host {
            Vec::new()
        } else {
            let methods = if host {
                &self.host
            } else {
                &self.guest
            };
            methods.iter().cloned()
                .map(|method|
                    self.show_(host, method.clone())
                ).collect()
        };
        let showing_guest = self.showing_guest.clone();
        async move {
            showing_guest.store(!host, Ordering::Relaxed);
            for method in methods {
                method.await?;
            }

            Ok(())
        }
    }

    fn show_(&self, host: bool, method: Arc<ConfigDdcMethod>) -> impl Future<Output=Result<(), Error>> {
        let input = if host {
            &self.input_host
        } else {
            &self.input_guest
        }.clone();
        let monitor = self.monitor.clone();
        let (ddc, qemu) = (
            self.ddc.clone(),
            self.qemu.clone(),
        );
        async move { match &*method {
            ConfigDdcMethod::GuestWait => Self::detect_guest_(&qemu).await,
            ConfigDdcMethod::Ddc | ConfigDdcMethod::Libddcutil | ConfigDdcMethod::Ddcutil => {
                tokio::task::spawn_blocking(move || {
                    let mut ddc = ddc.lock().unwrap();
                    let ddc = Self::ddc_connect(&mut ddc, &method, &monitor)?;
                    match input {
                        Some(input) =>
                            ddc.set_input(input),
                        None =>
                            Err(format_err!("DDC {} input source not found",
                                if host { "host" } else { "guest" }
                            )),
                    }
                }).await
                    .map_err(From::from).and_then(|r| r)
            },
            ConfigDdcMethod::Exec(args) => {
                let res = exec(args.iter()
                    .map(|i| Self::map_input_arg(i, input, host))
                    .collect::<Result<Vec<_>, Error>>()?
                ).into_future().await;
                res
            },
            ConfigDdcMethod::GuestExec(args) => {
                let res = qemu.guest_exec(args.iter()
                    .map(|i| Self::map_input_arg(i, input, host))
                    .collect::<Result<Vec<_>, Error>>()?
                ).into_future().await;
                res.map(drop)
            },
        } }
    }
}
