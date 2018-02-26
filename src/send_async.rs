use futures::future::Either;
use futures::task::{self, Task};
use futures::{Future, Stream, Sink, Async, Poll, IntoFuture};
use tokio_core::reactor::Handle;
use std::cell::RefCell;
use std::rc::Rc;

pub trait StreamUnzipExt {
    fn unzip<A, B>(self) -> (Unzip<Self, A, B>, UnzipStream<Self, A, B>) where Self: Sized + Stream<Item=(A, B)> {
        Unzip::new(self)
    }

    fn unzip_spawn<F: FnOnce(UnzipStream<Self, A, B>) -> U + 'static, U: IntoFuture<Item=(), Error=()> + 'static, A: 'static, B: 'static>(self, handle: &Handle, f: F) -> Unzip<Self, A, B> where Self: Sized + Stream<Item=(A, B)> + 'static {
        let (unzip, stream) = Unzip::new(self);

        handle.spawn_fn(|| f(stream));

        unzip
    }

    fn unzip_into<S: Sink<SinkItem=B, SinkError=()> + 'static, A: 'static, B: 'static>(self, handle: &Handle, sink: S) -> Unzip<Self, A, B> where Self: Sized + Stream<Item=(A, B)> + 'static {
        let (unzip, stream) = Unzip::new(self);

        handle.spawn(stream.forward(sink).map(drop));

        unzip
    }
}

impl<S: Stream> StreamUnzipExt for S { }

struct UnzipInner<S: Stream<Item=(A, B)>, A, B> {
    stream: S,
    error: Option<S::Error>,
    fused: bool,
    closed: (bool, bool),
    next: Option<Either<A, B>>,
    task: (Option<Task>, Option<Task>),
}

impl<S: Stream<Item=(A, B)>, A, B> UnzipInner<S, A, B> {
    fn stream_poll(&mut self, is_b: bool) -> Poll<Option<(A, B)>, S::Error> {
        match self.stream.poll() {
            res @ Ok(Async::Ready(None)) => {
                self.fused = true;
                self.notify(is_b);
                res
            },
            res @ Ok(Async::Ready(Some(..))) => {
                self.notify(is_b);
                res
            },
            res @ Ok(Async::NotReady) => {
                self.store_task(is_b);
                res
            },
            Err(err) => {
                self.fused = true;
                self.notify(is_b);
                if is_b {
                    self.error = Some(err);
                    Ok(Async::Ready(None))
                } else {
                    Err(err)
                }
            },
        }
    }

    #[inline]
    fn notify(&self, is_b: bool) {
        let task = if is_b { &self.task.0 } else { &self.task.1 };
        if let &Some(ref task) = task {
            task.notify()
        }
    }

    #[inline]
    fn store_task(&mut self, is_b: bool) {
        let task = if is_b { &mut self.task.1 } else { &mut self.task.0 };
        *task = Some(task::current());
    }
}

pub struct Unzip<S: Stream<Item=(A, B)>, A, B> {
    inner: Rc<RefCell<UnzipInner<S, A, B>>>,
}

pub struct UnzipStream<S: Stream<Item=(A, B)>, A, B> {
    inner: Rc<RefCell<UnzipInner<S, A, B>>>,
}

impl<S: Stream<Item=(A, B)>, A, B> Unzip<S, A, B> {
    pub fn new(stream: S) -> (Self, UnzipStream<S, A, B>) {
        let inner = Rc::new(RefCell::new(UnzipInner {
            stream: stream,
            error: None,
            fused: false,
            closed: (false, false),
            next: None,
            task: (None, None),
        }));

        (
            Unzip {
                inner: inner.clone(),
            },
            UnzipStream {
                inner: inner.clone(),
            },
        )
    }
}

impl<S: Stream<Item=(A, B)>, A, B> Stream for Unzip<S, A, B> {
    type Item = A;
    type Error = S::Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        let mut inner = self.inner.borrow_mut();
        if let Some(next) = inner.next.take() {
            return match next {
                Either::A(a) => {
                    inner.notify(false);
                    Ok(Async::Ready(Some(a)))
                },
                next => {
                    inner.next = Some(next);
                    inner.store_task(false);
                    Ok(Async::NotReady)
                },
            }
        } else if let Some(err) = inner.error.take() {
            inner.closed.0 = true;
            return Err(err)
        } else if inner.fused {
            return if inner.closed.0 {
                Ok(Async::NotReady)
            } else {
                inner.closed.0 = true;
                Ok(Async::Ready(None))
            }
        }

        match inner.stream_poll(false) {
            Ok(Async::Ready(None)) => {
                inner.closed.0 = true;
                Ok(Async::Ready(None))
            },
            Ok(Async::Ready(Some((a, b)))) => {
                inner.next = Some(Either::B(b));
                Ok(Async::Ready(Some(a)))
            }
            Ok(Async::NotReady) => {
                Ok(Async::NotReady)
            },
            Err(err) => {
                inner.closed.1 = true;
                Err(err)
            },
        }
    }
}

impl<S: Stream<Item=(A, B)>, A, B> Stream for UnzipStream<S, A, B> {
    type Item = B;
    type Error = ();

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        let mut inner = self.inner.borrow_mut();
        if let Some(next) = inner.next.take() {
            return match next {
                Either::B(b) => {
                    inner.notify(true);
                    Ok(Async::Ready(Some(b)))
                },
                next => {
                    inner.next = Some(next);
                    inner.store_task(true);
                    Ok(Async::NotReady)
                },
            }
        } else if inner.fused {
            return if inner.closed.1 {
                Ok(Async::NotReady)
            } else {
                inner.closed.1 = true;
                Ok(Async::Ready(None))
            }
        }

        match inner.stream_poll(true) {
            Ok(Async::Ready(None)) => {
                inner.closed.1 = true;
                Ok(Async::Ready(None))
            },
            Ok(Async::Ready(Some((a, b)))) => {
                inner.next = Some(Either::A(a));
                Ok(Async::Ready(Some(b)))
            }
            Ok(Async::NotReady) => {
                Ok(Async::NotReady)
            },
            Err(..) => unreachable!(),
        }
    }
}
