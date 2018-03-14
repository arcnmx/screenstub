struct Qemu {
    comm: ConfigQemuComm,
    driver: ConfigQemuDriver,
    qmp: Option<String>,
    ga: Option<String>,
    handle: Handle,
}

impl Qemu {
    pub fn new(qemu: config::ConfigQemu, handle: Handle) -> Self {
        Qemu {
            comm: qemu.comm,
            driver: qemu.driver,
            qmp: qemu.qmp_socket,
            ga: qemu.ga_socket,
            handle: handle,
        }
    }

    // TODO: none of these need to be mut probably?
    pub fn guest_exec<I: IntoIterator<Item=S>, S: AsRef<OsStr>>(&mut self, args: I) -> Box<Future<Item=(), Error=Error>> {
        match self.comm {
            ConfigQemuComm::None => {
                Box::new(future::ok(())) as Box<_>
            },
            ConfigQemuComm::Qemucomm => {
                if let Some(ga) = self.ga.as_ref() {
                    exec(&self.handle,
                         ["qemucomm", "-g", &ga, "exec", "-w"]
                            .iter().map(|&s| s.to_owned())
                        .chain(args.into_iter().map(|s| s.as_ref().to_string_lossy().into_owned()))
                    )
                } else {
                    Box::new(future::err(format_err!("QEMU Guest Agent socket not provided"))) as Box<_>
                }
            },
            ConfigQemuComm::QMP => {
                unimplemented!()
            },
            ConfigQemuComm::Console => {
                unimplemented!()
            },
        }
    }

    pub fn guest_info(&mut self) -> Box<Future<Item=(), Error=Error>> {
        match self.comm {
            ConfigQemuComm::None => {
                Box::new(future::ok(())) as Box<_>
            },
            ConfigQemuComm::Qemucomm => {
                if let Some(ga) = self.ga.as_ref() {
                    exec(&self.handle, ["qemucomm", "-g", &ga, "info"].iter().cloned())
                } else {
                    Box::new(future::err(format_err!("QEMU Guest Agent socket not provided"))) as Box<_>
                }
            },
            ConfigQemuComm::QMP => {
                unimplemented!()
            },
            ConfigQemuComm::Console => {
                unimplemented!()
            },
        }
    }

    pub fn add_evdev<I: AsRef<OsStr>, D: AsRef<OsStr>>(&mut self, id: I, device: D) -> Box<Future<Item=(), Error=Error>> {
        let device = format!("evdev={}", device.as_ref().to_string_lossy());

        match self.driver {
            ConfigQemuDriver::Virtio => self.add_device(id, "virtio-input-host", &[device]),
            ConfigQemuDriver::InputLinux => self.add_object(id, "input-linux", &[device]),
        }
    }

    pub fn add_object<I: AsRef<OsStr>, D: AsRef<OsStr>, PP: AsRef<OsStr>, P: IntoIterator<Item=PP>>(&mut self, id: I, driver: D, params: P) -> Box<Future<Item=(), Error=Error>> {
        let id = id.as_ref().to_string_lossy();
        let driver = driver.as_ref().to_string_lossy();

        match self.comm {
            ConfigQemuComm::None => {
                Box::new(future::ok(())) as Box<_>
            },
            ConfigQemuComm::Qemucomm => {
                if let Some(qmp) = self.qmp.as_ref() {
                    exec(&self.handle, ["qemucomm", "-q", &qmp, "add_object", &driver[..], &id[..]].iter()
                         .map(|&s| s.to_owned()).chain(params.into_iter().map(|p| p.as_ref().to_string_lossy().into_owned()))
                    )
                } else {
                    Box::new(future::err(format_err!("QEMU QMP socket not provided"))) as Box<_>
                }
            },
            ConfigQemuComm::QMP => {
                unimplemented!()
            },
            ConfigQemuComm::Console => {
                unimplemented!()
            },
        }
    }

    pub fn add_device<I: AsRef<OsStr>, D: AsRef<OsStr>, PP: AsRef<OsStr>, P: IntoIterator<Item=PP>>(&mut self, id: I, driver: D, params: P) -> Box<Future<Item=(), Error=Error>> {
        let id = id.as_ref().to_string_lossy();
        let driver = driver.as_ref().to_string_lossy();

        match self.comm {
            ConfigQemuComm::None => {
                Box::new(future::ok(())) as Box<_>
            },
            ConfigQemuComm::Qemucomm => {
                if let Some(qmp) = self.qmp.as_ref() {
                    exec(&self.handle, ["qemucomm", "-q", &qmp, "add_device", &driver[..], &id[..]].iter()
                         .map(|&s| s.to_owned()).chain(params.into_iter().map(|p| p.as_ref().to_string_lossy().into_owned()))
                    )
                } else {
                    Box::new(future::err(format_err!("QEMU QMP socket not provided"))) as Box<_>
                }
            },
            ConfigQemuComm::QMP => {
                unimplemented!()
            },
            ConfigQemuComm::Console => {
                unimplemented!()
            },
        }
    }

    pub fn remove_evdev<I: AsRef<OsStr>>(&mut self, id: I) -> Box<Future<Item=(), Error=Error>> {
        match self.driver {
            ConfigQemuDriver::Virtio => self.remove_device(id),
            ConfigQemuDriver::InputLinux => self.remove_object(id),
        }
    }

    pub fn guest_shutdown(&mut self, mode: QemuShutdownMode) -> Box<Future<Item=(), Error=Error>> {
        match self.comm {
            ConfigQemuComm::None => {
                Box::new(future::ok(())) as Box<_>
            },
            ConfigQemuComm::Qemucomm => {
                let mode = match mode {
                    QemuShutdownMode::Shutdown => None,
                    QemuShutdownMode::Reboot => Some("-r"),
                    QemuShutdownMode::Halt => Some("-h"),
                };

                if let Some(ga) = self.ga.as_ref() {
                    exec(&self.handle, ["qemucomm", "-g", &ga, "shutdown", mode.unwrap_or("ignore")].iter().cloned())
                } else {
                    Box::new(future::err(format_err!("QEMU QMP socket not provided"))) as Box<_>
                }
            },
            ConfigQemuComm::QMP => {
                unimplemented!()
            },
            ConfigQemuComm::Console => {
                unimplemented!()
            },
        }
    }

    pub fn remove_object<I: AsRef<OsStr>>(&mut self, id: I) -> Box<Future<Item=(), Error=Error>> {
        let id = id.as_ref().to_string_lossy();

        match self.comm {
            ConfigQemuComm::None => {
                Box::new(future::ok(())) as Box<_>
            },
            ConfigQemuComm::Qemucomm => {
                if let Some(qmp) = self.qmp.as_ref() {
                    exec(&self.handle, ["qemucomm", "-q", &qmp, "del_object", &id[..]].iter().cloned())
                } else {
                    Box::new(future::err(format_err!("QEMU QMP socket not provided"))) as Box<_>
                }
            },
            ConfigQemuComm::QMP => {
                unimplemented!()
            },
            ConfigQemuComm::Console => {
                unimplemented!()
            },
        }
    }

    pub fn remove_device<I: AsRef<OsStr>>(&mut self, id: I) -> Box<Future<Item=(), Error=Error>> {
        let id = id.as_ref().to_string_lossy();

        match self.comm {
            ConfigQemuComm::None => {
                Box::new(future::ok(())) as Box<_>
            },
            ConfigQemuComm::Qemucomm => {
                if let Some(qmp) = self.qmp.as_ref() {
                    exec(&self.handle, ["qemucomm", "-q", &qmp, "del_device", &id[..]].iter().cloned())
                } else {
                    Box::new(future::err(format_err!("QEMU QMP socket not provided"))) as Box<_>
                }
            },
            ConfigQemuComm::QMP => {
                unimplemented!()
            },
            ConfigQemuComm::Console => {
                unimplemented!()
            },
        }
    }

    pub fn set_is_mouse(&mut self, is_mouse: bool) -> Box<Future<Item=(), Error=Error>> {
        match self.driver {
            ConfigQemuDriver::Virtio => Box::new(future::ok(())) as Box<_>,
            ConfigQemuDriver::InputLinux => {
                const ID_MOUSE: &'static str = "screenstub-usbmouse";
                const ID_TABLET: &'static str = "screenstub-usbtablet";
                let (new, new_driver, old) = if is_mouse {
                    (ID_MOUSE, "usb-mouse", ID_TABLET)
                } else {
                    (ID_TABLET, "usb-tablet", ID_MOUSE)
                };

                let remove = self.remove_device(old);
                let add = self.add_device(new, new_driver, Vec::<String>::new());
                Box::new(
                    remove.or_else(|_| Ok(()))
                    .and_then(|_| add.or_else(|_| Ok(())))
                ) as Box<_>
            },
        }
    }
}

enum QemuShutdownMode {
    Shutdown,
    Reboot,
    Halt,
}
