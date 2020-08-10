use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use futures::lock::Mutex;
use tokio::time::{Duration, Instant, delay_until};
use failure::{Error, format_err};
use qemu::Qemu;
use config::{ConfigSource, ConfigMonitor, ConfigDdcMethod};
use crate::exec::exec;
use ddc::{SearchDisplay, DdcMonitor};

type DynMonitor = dyn DdcMonitor<Error=Error> + Send;

pub struct Sources {
    qemu: Arc<Qemu>,
    source_guest: Option<u8>,
    source_host: Option<u8>,
    target_showing: Arc<AtomicBool>,
    showing_guest: Arc<AtomicU8>,
    host: Vec<Arc<ConfigDdcMethod>>,
    guest: Vec<Arc<ConfigDdcMethod>>,
    monitor: Arc<SearchDisplay>,
    ddc: Arc<StdMutex<Option<Box<DynMonitor>>>>,
    throttle: Arc<Mutex<Instant>>,
    throttle_duration: Duration,
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

impl Sources {
    pub fn new(qemu: Arc<Qemu>, display: ConfigMonitor, source_host: ConfigSource, source_guest: ConfigSource, host: Vec<ConfigDdcMethod>, guest: Vec<ConfigDdcMethod>, throttle_duration: Duration) -> Self {
        Sources {
            qemu,
            source_guest: source_guest.value(),
            source_host: source_host.value(),
            target_showing: Arc::new(AtomicBool::new(false)),
            showing_guest: Arc::new(AtomicU8::new(2)),
            host: host.into_iter().map(Arc::new).collect(),
            guest: guest.into_iter().map(Arc::new).collect(),
            monitor: Arc::new(convert_display(display)),
            ddc: Arc::new(StdMutex::new(None)),
            throttle: Arc::new(Mutex::new(Instant::now() - throttle_duration)),
            throttle_duration,
        }
    }

    pub async fn fill(&mut self) -> Result<(), Error> {
        tokio::task::block_in_place(move || {
            let mut ddc = self.ddc.lock().unwrap();
            for method in &self.host {
                if self.source_host.is_some() && self.source_guest.is_some() {
                    break
                }
                let ddc = Self::ddc_connect(&mut ddc, method, &self.monitor)?;
                let source_host = match self.source_host {
                    Some(source) => source,
                    None => {
                        let source = ddc.get_source()?;
                        self.source_host = Some(source);
                        source
                    },
                };
                match &self.source_guest {
                    Some(..) => (),
                    None =>
                        self.source_guest = ddc.find_guest_source(source_host)?,
                }
            }

            Ok(())
        })
    }

    fn map_source_arg<S: AsRef<str>>(s: S, source: Option<u8>, host: bool) -> Result<String, Error> {
        let source = source
            .ok_or_else(|| format_err!("DDC {} source not found",
                if host { "host" } else { "guest" }
            ));
        let s = s.as_ref();
        Ok(if s == "{}" {
            format!("{}", source?)
        } else if s == "{:x}" {
            format!("{:02x}", source?)
        } else if s == "0x{:x}" {
            format!("0x{:02x}", source?)
        } else {
            s.to_owned()
        })
    }

    // TODO: detect current showing state via ddc when unknown?

    pub fn showing_guest(&self) -> Option<bool> {
        Self::showing_guest_(&self.showing_guest)
    }

    fn showing_guest_(showing_guest: &AtomicU8) -> Option<bool> {
        match showing_guest.load(Ordering::Relaxed) {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        }
    }

    pub fn show_guest(&self) -> impl Future<Output=Result<(), Error>> {
        self.show(false, false)
    }

    pub fn show_host(&self) -> impl Future<Output=Result<(), Error>> {
        self.show(true, false)
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

    pub fn show(&self, host: bool, force: bool) -> impl Future<Output=Result<(), Error>> {
        let show_host = self.show_commands(true);
        let show_guest = self.show_commands(false);

        self.target_showing.store(host, Ordering::Relaxed);

        let target_showing = self.target_showing.clone();
        let showing_guest = self.showing_guest.clone();
        let throttle = self.throttle.clone();
        let throttle_duration = self.throttle_duration;
        async move {
            let mut throttle = throttle.lock().await;

            let throttle_until = *throttle;
            let now = Instant::now();
            let throttle_until = if throttle_until <= now {
                // a short delay gives the event loop a chance to change its mind
                now + Duration::from_millis(48)
            } else {
                throttle_until
            };
            delay_until(throttle_until).await;

            let host = target_showing.load(Ordering::Relaxed);
            let guest = !host;

            if force || Self::showing_guest_(&showing_guest) != Some(guest) {
                let methods = if host {
                    show_host
                } else {
                    show_guest
                };

                for method in methods {
                    method.await?;
                }

                showing_guest.store(guest as u8, Ordering::Relaxed);
                *throttle = Instant::now() + throttle_duration;
            }

            Ok(())
        }
    }

    fn show_commands(&self, host: bool) -> Vec<impl Future<Output=Result<(), Error>>> {
        let methods = if host {
            &self.host
        } else {
            &self.guest
        };
        methods.iter().cloned()
            .map(|method|
                self.show_(host, method)
            ).collect()
    }

    fn show_(&self, host: bool, method: Arc<ConfigDdcMethod>) -> impl Future<Output=Result<(), Error>> {
        let source = if host {
            &self.source_host
        } else {
            &self.source_guest
        }.clone();
        let monitor = self.monitor.clone();
        let (ddc, qemu) = (
            self.ddc.clone(),
            self.qemu.clone(),
        );
        async move { match &*method {
            ConfigDdcMethod::GuestWait => qemu.guest_wait().await,
            ConfigDdcMethod::Ddc | ConfigDdcMethod::Libddcutil | ConfigDdcMethod::Ddcutil => {
                tokio::task::spawn_blocking(move || {
                    let mut ddc = ddc.lock().unwrap();
                    let ddc = Self::ddc_connect(&mut ddc, &method, &monitor)?;
                    match source {
                        Some(source) =>
                            ddc.set_source(source),
                        None =>
                            Err(format_err!("DDC {} source not found",
                                if host { "host" } else { "guest" }
                            )),
                    }
                }).await
                    .map_err(From::from).and_then(|r| r)
            },
            ConfigDdcMethod::Exec(args) => {
                let res = exec(args.iter()
                    .map(|i| Self::map_source_arg(i, source, host))
                    .collect::<Result<Vec<_>, Error>>()?
                ).into_future().await;
                res
            },
            ConfigDdcMethod::GuestExec(args) => {
                let res = qemu.guest_exec(args.iter()
                    .map(|i| Self::map_source_arg(i, source, host))
                    .collect::<Result<Vec<_>, Error>>()?
                ).into_future().await;
                res.map(drop)
            },
        } }
    }
}
