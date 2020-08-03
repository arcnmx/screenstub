use std::io;
use std::sync::{Mutex, Arc, Weak};
use std::future::Future;
use failure::{Error, format_err};
use futures::{TryFutureExt, StreamExt};
use futures::future;
use tokio::net::UnixStream;
use tokio::time::{Duration, Instant, delay_for, timeout};
use tokio::io::{ReadHalf, WriteHalf};
use tokio::sync::broadcast;
use log::{trace, warn, info};

pub struct Qemu {
    socket_qmp: Option<String>,
    socket_qga: Option<String>,
    qmp: Mutex<Weak<QmpService>>,
    event_send: broadcast::Sender<qapi::qmp::Event>,
    connection_lock: futures::lock::Mutex<()>,
}

type QgaWrite = qapi::futures::QgaStreamTokio<WriteHalf<UnixStream>>;
type QmpRead = qapi::futures::QmpStreamTokio<ReadHalf<UnixStream>>;
type QmpWrite = qapi::futures::QmpStreamTokio<WriteHalf<UnixStream>>;
pub type QgaService = qapi::futures::QapiService<QgaWrite>;
pub type QmpService = qapi::futures::QapiService<QmpWrite>;
pub type QmpStream = qapi::futures::QapiStream<QmpRead, QmpWrite>;
pub type QmpEvents = qapi::futures::QapiEvents<QmpRead>;

impl Qemu {
    pub fn new(socket_qmp: Option<String>, socket_qga: Option<String>) -> Self {
        let (event_send, _event_recv) = broadcast::channel(8);
        Qemu {
            socket_qmp,
            socket_qga,
            event_send,
            qmp: Mutex::new(Weak::new()),
            connection_lock: Default::default(),
        }
    }

    pub fn qmp_events(&self) -> broadcast::Receiver<qapi::qmp::Event> {
        self.event_send.subscribe()
    }

    pub async fn connect_qmp(&self) -> Result<Arc<QmpService>, Error> {
        let _lock = self.connection_lock.lock().await;
        let qmp = self.qmp.lock().unwrap().upgrade();
        match qmp {
            Some(res) => Ok(res),
            None => {
                let stream = self.connect_qmp_stream().await?;
                let mut qmp = self.qmp.lock().unwrap();
                let (stream, mut events) = stream.into_parts();
                Ok(match qmp.upgrade() {
                    // if two threads fight for this, just ditch this new connection
                    Some(qmp) => qmp,
                    None => {
                        let res = Arc::new(stream);
                        *qmp = Arc::downgrade(&res);
                        let event_send = self.event_send.clone();
                        let _ = events.release();
                        tokio::spawn(async move {
                            while let Some(event) = events.next().await {
                                match event {
                                    Ok(e) => match event_send.send(e) {
                                        Err(e) => {
                                            info!("QMP event ignored: {:?}", e.0);
                                        },
                                        Ok(_) => (),
                                    },
                                    Err(e) => {
                                        warn!("QMP stream error: {:?}", e);
                                        break
                                    },
                                }
                            }
                        });
                        res
                    },
                })
            },
        }
    }

    fn connect_qmp_stream(&self) -> impl Future<Output=Result<QmpStream, io::Error>> {
        let socket_qmp = self.socket_qmp.as_ref()
            .ok_or_else(|| io::Error::new(io::ErrorKind::AddrNotAvailable, "QMP socket not configured"))
            .map(|p| p.to_owned());

        async move {
            let stream = qapi::futures::QmpStreamTokio::open_uds(socket_qmp?).await?;
            stream.negotiate().await
        }
    }

    pub fn connect_qga(&self) -> impl Future<Output=Result<QgaService, io::Error>> {
        let socket_qga = self.socket_qga.as_ref()
            .ok_or_else(|| io::Error::new(io::ErrorKind::AddrNotAvailable, "QGA socket not configured"))
            .map(|p| p.to_owned());

        async move {
            let stream = qapi::futures::QgaStreamTokio::open_uds(socket_qga?).await?;
            let (service, _) = stream.spawn_tokio();

            Ok(service)
        }
    }

    pub async fn execute_qga<C: qapi::qga::QgaCommand>(&self, command: C) -> qapi::ExecuteResult<C> {
        let qga = self.connect_qga().await?;
        qga.execute(command).await
    }

    pub async fn execute_qmp<C: qapi::qmp::QmpCommand>(&self, command: C) -> Result<C::Ok, Error> {
        self.connect_qmp().await?.execute(command).await
            .map_err(From::from)
    }

    pub async fn device_add(&self, add: qapi::qmp::device_add, deadline: Instant) -> Result<(), Error> {
        let qmp = self.connect_qmp().await?;
        let id = add.id.as_ref()
            .ok_or_else(|| format_err!("device_add id not found"))?
            .to_owned();
        let path = format!("/machine/peripheral/{}", id);
        let exists = match qmp.execute(qapi::qmp::qom_list { path }).await {
            Ok(_) => true,
            Err(qapi::ExecuteError::Qapi(e)) if matches!(e.class, qapi::ErrorClass::DeviceNotFound) => false,
            Err(e) => return Err(e.into()),
        };
        if exists {
            let mut events = self.qmp_events();
            let delete = qmp.execute(qapi::qmp::device_del { id: id.clone() })
                .map_err(Error::from)
                .map_ok(drop);
            let wait = async move {
                while let Some(e) = events.next().await {
                    match e? {
                        qapi::qmp::Event::DEVICE_DELETED { ref data, .. } if data.device.as_ref() == Some(&id) => return Ok(()),
                        _ => (),
                    }
                }
                Err(format_err!("Expected DEVICE_DELETED event"))
            };
            future::try_join(delete, wait).await?;
        }

        tokio::time::delay_until(deadline).await;
        qmp.execute(add).await?;

        Ok(())
    }

    pub fn guest_exec_(&self, exec: qapi::qga::guest_exec) -> impl Future<Output=Result<qapi::qga::GuestExecStatus, Error>> {
        let connect = self.connect_qga();
        async move {
            trace!("QEMU GA Exec {:?}", exec);

            let qga = connect.await?;
            match qga.execute(exec).await {
                Ok(qapi::qga::GuestExec { pid }) => loop {
                    match qga.execute(qapi::qga::guest_exec_status { pid }).await {
                        Ok(r) if !r.exited => delay_for(Duration::from_millis(100)).await,
                        res => break res.map_err(From::from),
                    }
                },
                Err(e) => Err(e.into()),
            }
        }
    }

    pub fn guest_exec<I: IntoIterator<Item=S>, S: Into<String>>(&self, args: I) -> GuestExec {
        let mut args = args.into_iter();
        let cmd = args.next().expect("at least one command argument expected");

        let exec = qapi::qga::guest_exec {
            path: cmd.into(),
            arg: Some(args.map(Into::into).collect()),
            env: Default::default(),
            input_data: Default::default(),
            capture_output: Some(true),
        };

        GuestExec {
            qemu: self,
            exec,
        }
    }

    pub fn guest_shutdown(&self, shutdown: qapi::qga::guest_shutdown) -> impl Future<Output=Result<(), Error>> {
        // TODO: a shutdown (but not reboot) can be verified waiting for exit event or socket close or with --no-shutdown, query-status is "shutdown". Ugh!

        let connect = self.connect_qga();
        async move {
            let qga = connect.await?;
            match timeout(Duration::from_secs(1), qga.execute(shutdown)).await {
                Ok(res) => res.map(drop).map_err(From::from),
                Err(_) => {
                    warn!("Shutdown response timed out");
                    Ok(())
                },
            }
        }
    }
}

pub struct GuestExec<'a> {
    qemu: &'a Qemu,
    exec: qapi::qga::guest_exec,
}

impl<'a> GuestExec<'a> {
    pub fn into_future(self) -> impl Future<Output=Result<qapi::qga::GuestExecStatus, Error>> {
        self.qemu.guest_exec_(self.exec)
    }
}
