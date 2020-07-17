use std::{io, ops, os::unix::net};
use std::pin::Pin;
use std::task::{Context, Waker, Poll};
use std::sync::{Mutex, Arc, Weak};
use std::time::Duration;
use std::future::Future;
use failure::Error;
use futures::{Stream, TryFutureExt, FutureExt, poll};
use futures::future::{AbortHandle, abortable};
use futures::stream::FusedStream;
use tokio::net::UnixStream;
use tokio::time::delay_for;
use tokio::io::{ReadHalf, WriteHalf};
use tokio::pin;
use tokio_qapi::qmp::QapiCapabilities;
use tokio_qapi::{
    Command,
    QapiEvents, QapiStream,
    qga, qmp,
};
use log::trace;

pub struct Qemu {
    socket_qmp: Option<String>,
    socket_qga: Option<String>,
    qmp: Mutex<Weak<QmpHandle>>,
    qmp_waker: Mutex<Option<Waker>>,
    connection_lock: futures::lock::Mutex<()>,
}

pub struct QmpHandle {
    stream: QemuStream,
    events: Mutex<Pin<Box<dyn FusedStream<Item=io::Result<qmp::Event>> + Send>>>,
}

impl<'a> Stream for &'a QmpHandle {
    type Item = io::Result<qmp::Event>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        let mut events = self.events.lock().unwrap();
        events.as_mut().poll_next(cx)
    }
}

impl<'a> FusedStream for &'a QmpHandle {
    fn is_terminated(&self) -> bool {
        self.events.lock().unwrap().is_terminated()
    }
}

impl ops::Deref for QmpHandle {
    type Target = QemuStream;

    fn deref(&self) -> &Self::Target {
        &self.stream
    }
}

pub type QemuStreamRead = ReadHalf<UnixStream>;
pub type QemuStreamWrite = WriteHalf<UnixStream>;
pub type QemuStream = QapiStream<QemuStreamWrite>;
pub type QemuEvents = QapiEvents<QemuStreamRead>;

impl Qemu {
    pub fn new(socket_qmp: Option<String>, socket_qga: Option<String>) -> Self {
        Qemu {
            socket_qmp,
            socket_qga,
            qmp: Mutex::new(Weak::new()),
            qmp_waker: Mutex::new(None),
            connection_lock: Default::default(),
        }
    }

    pub fn poll_qmp_events(&self, cx: &mut Context) -> Poll<Option<io::Result<qmp::Event>>> {
        match self.qmp.lock().unwrap().upgrade() {
            Some(qmp) => {
                Pin::new(&mut &*qmp).poll_next(cx)
            },
            None => {
                *self.qmp_waker.lock().unwrap() = Some(cx.waker().clone());
                Poll::Pending
            },
        }
    }

    pub async fn qmp_clone(&self) -> Result<Arc<QmpHandle>, Error> {
        let _lock = self.connection_lock.lock().await;
        let qmp = self.qmp.lock().unwrap().upgrade();
        match qmp {
            Some(res) => Ok(res),
            None => {
                let (_caps, stream, events) = self.connect_qmp_events().await?;
                let mut qmp = self.qmp.lock().unwrap();
                Ok(match qmp.upgrade() {
                    // if two threads fight for this, just ditch this new connection
                    Some(qmp) => qmp,
                    None => {
                        let res = Arc::new(QmpHandle {
                            stream,
                            events: Mutex::new(Box::pin(events.into_stream())),
                        });
                        *qmp = Arc::downgrade(&res);
                        if let Some(waker) = self.qmp_waker.lock().unwrap().take() {
                            waker.wake();
                        }
                        res
                    },
                })
            },
        }
    }

    pub async fn qmp_events(&self) -> Result<QemuEvents, io::Error> {
        self.connect_qmp_events()
            .map_ok(|(_caps, _stream, events)| events).await
    }

    pub fn connect_qmp_events(&self) -> impl Future<Output=Result<(QapiCapabilities, QemuStream, QemuEvents), io::Error>> {
        let socket_qmp = self.socket_qmp.as_ref()
            .ok_or_else(|| io::Error::new(io::ErrorKind::AddrNotAvailable, "QMP socket not configured"))
            .and_then(|path| net::UnixStream::connect(path));

        async move {
            let socket = UnixStream::from_std(socket_qmp?)?;
            QapiStream::open_tokio(socket).await
        }
    }

    async fn connect_qmp_(&self) -> Result<(QapiCapabilities, QemuStream, AbortHandle), io::Error> {
        let (caps, stream, events) = self.connect_qmp_events().await?;

        let (events, abort) = abortable(events.spin());

        tokio::spawn(events.map_err(drop).boxed());

        Ok((caps, stream, abort))
    }

    pub async fn connect_qmp(&self) -> Result<(QemuStream, AbortHandle), io::Error> {
        self.connect_qmp_()
            .map_ok(|(_caps, stream, abort)| (stream, abort)).await
    }

    fn connect_qga_(&self) -> impl Future<Output=Result<(QemuStream, AbortHandle), io::Error>> {
        let socket_qga = self.socket_qga.as_ref()
            .ok_or_else(|| io::Error::new(io::ErrorKind::AddrNotAvailable, "QGA socket not configured"))
            .and_then(|path| net::UnixStream::connect(path));

        async move {
            let socket = UnixStream::from_std(socket_qga?)?;
            let (stream, events) = QapiStream::open_tokio_qga(socket).await?;

            let (events, abort) = abortable(events);

            tokio::spawn(events.map_err(drop).boxed());

            Ok((stream, abort))
        }
    }

    pub async fn connect_qga(&self) -> Result<(QemuStream, AbortHandle), io::Error> {
        self.connect_qga_()
            .map_ok(|(stream, abort)| (stream, abort)).await
    }

    /*pub fn qmp_events(&self) -> Box<Future<Item=QapiEventStream<QemuStream>, Error=Error>> {
        Box::new(future::result(
                self.socket_qga.as_ref().ok_or_else(|| io::Error::new(io::ErrorKind::AddrNotAvailable, "QGA socket not configured"))
                .and_then(|socket| UnixStream::connect(socket))
                .map_err(Error::from)
            ).map(qapi::event_stream)
            .and_then(|(s, e)|
                qapi::qmp_handshake(s).map(|_s| e)
                .map_err(Error::from)
            )
        ) as Box<_>
    }*/

    /*fn connect_qmp_(&self) -> io::Result<QmpHandshake<QemuStream>> {
        if let Some(ref socket) = self.socket_qmp {
            let stream = UnixStream::connect(socket, handle)?;
            Ok(qapi::qmp_handshake(qapi::stream(stream)))
        } else {
            Err(io::Error::new(io::ErrorKind::AddrNotAvailable, "QMP socket not configured"))
        }
    }

    fn connect_qga_(&self) -> io::Result<QgaHandshake<QemuStream>> {
        if let Some(ref socket) = self.socket_qga {
            let stream = UnixStream::connect(socket, handle)?;
            Ok(qapi::qga_handshake(qapi::stream(stream)))
        } else {
            Err(io::Error::new(io::ErrorKind::AddrNotAvailable, "QGA socket not configured"))
        }
    }*/

    /*pub fn connect_qga(&self) -> QgaFuture {
        ConnectFuture::new(self.connect_qga_(handle))
    }*/

    pub async fn execute_qga<C: Command + 'static>(&self, command: C) -> Result<C::Ok, io::Error> {
        let (qga, abort) = self.connect_qga_().await?;
        let res = qga.execute(command).await;
        abort.abort();
        res
            .map_err(From::from)
            .and_then(|r| r.map_err(From::from))
    }

    pub async fn execute_qmp<C: Command + 'static>(&self, command: C) -> Result<C::Ok, Error> {
        /*let (_caps, qmp, abort) = self.connect_qmp_().await?;
        let res = qmp.execute(command).await;
        abort.abort();
        res
            .map_err(From::from)
            .and_then(|r| r.map_err(From::from))*/
        self.qmp_clone().await?.execute(command).await
            .map_err(From::from)
            .and_then(|r| r.map_err(From::from))
    }

    pub fn guest_exec_(&self, exec: qga::guest_exec) -> impl Future<Output=Result<qga::GuestExecStatus, Error>> {
        let connect = self.connect_qga_();
        async move {
            trace!("QEMU GA Exec {:?}", exec);

            let (qga, abort) = connect.await?;
            let res = match qga.execute(exec).await {
                Ok(Ok(qga::GuestExec { pid })) => loop {
                    let res = qga.execute(qga::guest_exec_status { pid }).await
                        .map_err(From::from)
                        .and_then(|r| r.map_err(From::from));
                    match res {
                        Ok(r) if !r.exited => delay_for(Duration::from_millis(100)).await,
                        res => break res,
                    }
                },
                Ok(Err(e)) => Err(e.into()),
                Err(e) => Err(e.into()),
            };

            abort.abort();
            res
        }
    }

    pub fn guest_exec<I: IntoIterator<Item=S>, S: Into<String>>(&self, args: I) -> GuestExec {
        let mut args = args.into_iter();
        let cmd = args.next().expect("at least one command argument expected");

        let exec = qga::guest_exec {
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

    pub fn guest_shutdown(&self, shutdown: qga::guest_shutdown) -> impl Future<Output=Result<(), Error>> {
        let connect = self.connect_qga_();
        async move {
            let (qga, abort) = connect.await?;
            let res = qga.execute(shutdown);
            pin!(res);

            // attempt a single poll of the connection to check for an error, otherwise don't wait
            // TODO: a shutdown (but not reboot) can be verified waiting for exit event or socket close or with --no-shutdown, query-status is "shutdown". Ugh!
            let res = match poll!(res) {
                Poll::Ready(Err(e)) => Err(e.into()),
                Poll::Ready(Ok(Err(e))) => Err(e.into()),
                _ => Ok(()),
            };

            abort.abort();
            res
        }
    }
}

pub struct GuestExec<'a> {
    qemu: &'a Qemu,
    exec: qga::guest_exec,
}

impl<'a> GuestExec<'a> {
    pub fn into_future(self) -> impl Future<Output=Result<qga::GuestExecStatus, Error>> {
        self.qemu.guest_exec_(self.exec)
    }
}
