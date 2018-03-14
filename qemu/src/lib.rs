extern crate tokio_qapi as qapi;
extern crate tokio_uds;
extern crate tokio_core;
#[macro_use]
extern crate futures;
extern crate failure;
#[macro_use]
extern crate log;

use std::{io, mem};
use failure::Error;
use futures::{Future, Stream, Sink, Poll, Async, future};
use tokio_core::reactor::Handle;
use tokio_uds::UnixStream;
use qapi::{
    Command,
    QapiDataStream, QapiStream, QapiEventStream, QapiFuture,
    QmpHandshake, QgaHandshake,
    qga, qmp,
};

pub struct Qemu {
    socket_qmp: Option<String>,
    socket_qga: Option<String>,
}

pub type QemuStream = QapiDataStream<UnixStream>;

pub type QgaFuture = ConnectFuture<QgaHandshake<QemuStream>, io::Error>;
pub type QmpFuture = QmpConnectResult<ConnectFuture<QmpHandshake<QemuStream>, io::Error>>;
pub type QemuFuture<F, C> = CommandFuture<F, QemuStream, C>;

impl Qemu {
    pub fn new(socket_qmp: Option<String>, socket_qga: Option<String>) -> Self {
        Qemu {
            socket_qmp: socket_qmp,
            socket_qga: socket_qga,
        }
    }

    pub fn qmp_events(&self, handle: &Handle) -> Box<Future<Item=QapiEventStream<QemuStream>, Error=Error>> {
        Box::new(future::result(
                self.socket_qga.as_ref().ok_or_else(|| io::Error::new(io::ErrorKind::AddrNotAvailable, "QGA socket not configured"))
                .and_then(|socket| UnixStream::connect(socket, handle))
                .map_err(Error::from)
            ).map(qapi::event_stream)
            .and_then(|(s, e)|
                qapi::qmp_handshake(s).map(|_s| e)
                .map_err(Error::from)
            )
        ) as Box<_>
    }

    fn connect_qmp_(&self, handle: &Handle) -> io::Result<QmpHandshake<QemuStream>> {
        if let Some(ref socket) = self.socket_qmp {
            let stream = UnixStream::connect(socket, handle)?;
            Ok(qapi::qmp_handshake(qapi::stream(stream)))
        } else {
            Err(io::Error::new(io::ErrorKind::AddrNotAvailable, "QMP socket not configured"))
        }
    }

    fn connect_qga_(&self, handle: &Handle) -> io::Result<QgaHandshake<QemuStream>> {
        if let Some(ref socket) = self.socket_qga {
            let stream = UnixStream::connect(socket, handle)?;
            Ok(qapi::qga_handshake(qapi::stream(stream)))
        } else {
            Err(io::Error::new(io::ErrorKind::AddrNotAvailable, "QGA socket not configured"))
        }
    }

    pub fn connect_qga(&self, handle: &Handle) -> QgaFuture {
        ConnectFuture::new(self.connect_qga_(handle))
    }

    pub fn connect_qmp(&self, handle: &Handle) -> QmpFuture {
        QmpConnectResult::new(ConnectFuture::new(self.connect_qmp_(handle)))
    }

    pub fn execute_qga<C: Command + 'static>(&self, handle: &Handle, command: C) -> ResultFuture<QemuFuture<QgaFuture, C>> {
        ResultFuture::new(CommandFuture::new(self.connect_qga(handle), command))
    }

    pub fn execute_qmp<C: Command + 'static>(&self, handle: &Handle, command: C) -> ResultFuture<QemuFuture<QmpFuture, C>> {
        ResultFuture::new(CommandFuture::new(self.connect_qmp(handle), command))
    }

    pub fn guest_exec<I: IntoIterator<Item=S>, S: Into<String>>(&self, handle: &Handle, args: I) -> Box<Future<Item=qga::GuestExecStatus, Error=Error>> {
        use futures::future::{Loop, Either};

        let mut args = args.into_iter();
        let cmd = args.next().expect("at least one command argument expected");

        let exec = qga::guest_exec {
            path: cmd.into(),
            arg: Some(args.map(Into::into).collect()),
            env: Default::default(),
            input_data: Default::default(),
            capture_output: Some(true),
        };

        trace!("QEMU GA Exec {:?}", exec);

        Box::new(
            CommandFuture::new(self.connect_qga(handle), exec)
                .and_then(|(qga::GuestExec { pid }, s)| future::loop_fn((s, None::<qga::GuestExecStatus>), move |(s, st)| Either::B(match st {
                    None => s,
                    Some(ref st) if !st.exited => s,
                    Some(st) => return Either::A(future::ok(Loop::Break(st))),
                }.execute(qga::guest_exec_status { pid: pid })
                    .map_err(From::from)
                    .and_then(|(r, s)| r.map(|r| (r, s)).map_err(From::from))
                    .map(|(st, s)| Loop::Continue((s, Some(st))))
                ))).and_then(|st| st.result().map_err(Error::from))
        ) as Box<_>
    }

    pub fn guest_shutdown(&self, handle: &Handle, shutdown: qga::guest_shutdown) -> Box<Future<Item=(), Error=Error>> {
        Box::new(self.connect_qga(handle)
            .map_err(Error::from)
            .and_then(move |s| qapi::encode_command(&shutdown).map(|c| (s, c)).map_err(Error::from))
            .and_then(|(s, c)| s.send(c).map_err(Error::from))
            // TODO: attempt a single poll of the connection to check for an error, otherwise don't wait
            // TODO: a shutdown (but not reboot) can be verified waiting for exit event or socket close or with --no-shutdown, query-status is "shutdown". Ugh!
            .map(drop)) as Box<_>
    }
}

pub struct QmpConnectResult<F> {
    future: F,
}

impl<S, F> Future for QmpConnectResult<F> where
    F: Future<Item=(qmp::QMP, S)>,
{
    type Item = S;
    type Error = F::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let (_, s) = try_ready!(self.future.poll());
        Ok(Async::Ready(s))
    }
}

impl<F> QmpConnectResult<F> {
    pub fn new(f: F) -> Self {
        QmpConnectResult {
            future: f,
        }
    }
}

pub struct ConnectFuture<F, E> {
    future: Option<Result<F, E>>,
}

impl<F, E> ConnectFuture<F, E> {
    fn new(f: Result<F, E>) -> Self {
        ConnectFuture {
            future: Some(f),
        }
    }
}

impl<E, F> Future for ConnectFuture<F, E> where
    F: Future,
    F::Error: From<E>,
{
    type Item = F::Item;
    type Error = F::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let mut future = self.future.take().expect("polled after completion")?;
        let res = future.poll();
        self.future = Some(Ok(future));
        res
    }
}

pub struct ResultFuture<F> {
    future: F,
}

impl<F> ResultFuture<F> {
    fn new(f: F) -> Self {
        ResultFuture {
            future: f,
        }
    }
}

impl<I, S, F: Future<Item=(I, S)>> Future for ResultFuture<F> {
    type Item = I;
    type Error = F::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let (res, _) = try_ready!(self.future.poll());

        Ok(Async::Ready(res))
    }
}

pub enum CommandFuture<F, S, C: Command> {
    Stream {
        future: F,
        command: C,
    },
    Command {
        future: QapiFuture<C, QapiStream<S>>,
    },
    None,
}

impl<F, S, C: Command> CommandFuture<F, S, C> {
    pub fn new(f: F, c: C) -> Self {
        CommandFuture::Stream {
            future: f,
            command: c,
        }
    }
}

impl<SE, C, S, F> Future for CommandFuture<F, S, C> where
    C: Command,
    F: Future<Item=QapiStream<S>>,
    SE: From<io::Error>,
    S: Stream<Error=SE> + Sink<SinkItem=Box<[u8]>, SinkError=SE>,
    S::Item: AsRef<[u8]>,
    io::Error: From<S::Error>,
    Error: From<F::Error>,
{
    type Item = (C::Ok, QapiStream<S>);
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let stream = match *self {
            CommandFuture::Stream { ref mut future, .. } => try_ready!(future.poll()),
            CommandFuture::Command { ref mut future } => {
                let (res, stream) = try_ready!(future.poll());
                let res: io::Result<_> = res.map_err(From::from);
                return res.map(|res| Async::Ready((res, stream))).map_err(From::from)
            },
            CommandFuture::None => unreachable!(),
        };

        let command = match mem::replace(self, CommandFuture::None) {
            CommandFuture::Stream { command, .. } => command,
            _ => unreachable!(),
        };

        let mut future = stream.execute(command);
        let res = future.poll();
        *self = CommandFuture::Command {
            future: future,
        };

        let (res, stream) = try_ready!(res);
        res.map(|res| Async::Ready((res, stream))).map_err(From::from)
    }
}
