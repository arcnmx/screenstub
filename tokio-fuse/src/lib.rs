extern crate futures;

use std::rc::Rc;
use std::cell::RefCell;
use futures::{Stream, Sink, StartSend, Poll, Async};
use futures::task::{self, Task};

pub struct Fuse<S> {
    inner: Option<S>,
    task: Option<Task>,
}

impl<S> Fuse<S> {
    pub fn new(inner: S) -> Self {
        Fuse {
            inner: Some(inner),
            task: None,
        }
    }

    pub fn inner(&self) -> Option<&S> {
        self.inner.as_ref()
    }

    pub fn inner_mut(&mut self) -> Option<&mut S> {
        self.inner.as_mut()
    }

    pub fn into_inner(self) -> Option<S> {
        self.inner
    }

    pub fn fuse_inner(&mut self) {
        self.inner = None;
        if let Some(ref task) = self.task {
            task.notify()
        }
    }
}

impl<S: Sink> Sink for Fuse<S> {
    type SinkItem = S::SinkItem;
    type SinkError = S::SinkError;

    fn start_send(&mut self, item: S::SinkItem) -> StartSend<S::SinkItem, S::SinkError> {
        if let Some(ref mut inner) = self.inner {
            inner.start_send(item)
        } else {
            unimplemented!()
        }
    }

    fn poll_complete(&mut self) -> Poll<(), S::SinkError> {
        if let Some(ref mut inner) = self.inner {
            inner.poll_complete()
        } else {
            unimplemented!()
        }
    }

    fn close(&mut self) -> Poll<(), S::SinkError> {
        if let Some(ref mut inner) = self.inner {
            inner.close()
        } else {
            Ok(Async::Ready(()))
        }
    }
}

impl <S: Stream> Stream for Fuse<S> {
    type Item = S::Item;
    type Error = S::Error;

    fn poll(&mut self) -> Poll<Option<S::Item>, S::Error> {
        self.task = Some(task::current());

        if let Some(mut inner) = self.inner.take() {
            match inner.poll() {
                r @ Ok(Async::Ready(None)) => r,
                r => {
                    self.inner = Some(inner);
                    r
                },
            }
        } else {
            Ok(Async::Ready(None))
        }
    }
}

pub struct SharedFuse<S> {
    inner: Rc<RefCell<Fuse<S>>>,
}

impl<S> Clone for SharedFuse<S> {
    fn clone(&self) -> Self {
        SharedFuse {
            inner: self.inner.clone(),
        }
    }
}

impl<S> SharedFuse<S> {
    pub fn new(inner: S) -> Self {
        SharedFuse {
            inner: Rc::new(RefCell::new(Fuse::new(inner))),
        }
    }

    pub fn inner<U, F: FnOnce(Option<&S>) -> U>(&self, f: F) -> U {
        f(self.inner.borrow().inner())
    }

    pub fn inner_mut<U, F: FnOnce(Option<&mut S>) -> U>(&mut self, f: F) -> U {
        f(self.inner.borrow_mut().inner_mut())
    }

    pub fn into_inner(self) -> Rc<RefCell<Fuse<S>>> {
        self.inner
    }

    pub fn fuse_inner(&mut self) {
        self.inner.borrow_mut().fuse_inner()
    }
}

impl<S: Stream> Stream for SharedFuse<S> {
    type Item = S::Item;
    type Error = S::Error;

    fn poll(&mut self) -> Poll<Option<S::Item>, S::Error> {
        self.inner.borrow_mut().poll()
    }
}

impl<S: Sink> Sink for SharedFuse<S> {
    type SinkItem = S::SinkItem;
    type SinkError = S::SinkError;

    fn start_send(&mut self, item: S::SinkItem) -> StartSend<S::SinkItem, S::SinkError> {
        self.inner.borrow_mut().start_send(item)
    }

    fn poll_complete(&mut self) -> Poll<(), S::SinkError> {
        self.inner.borrow_mut().poll_complete()
    }

    fn close(&mut self) -> Poll<(), S::SinkError> {
        self.inner.borrow_mut().close()
    }
}
