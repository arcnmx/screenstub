extern crate mio;

use std::fs::OpenOptions;
use std::io::{self, Read, Write};
use self::mio::unix::EventedFd;
use self::mio::event::Evented;
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;

pub struct Fd<T> {
	fd: RawFd,
	inner: T,
}

pub fn open_options() -> OpenOptions {
	let mut o = OpenOptions::new();
	o.read(true);
	o.custom_flags(0o4000); // O_NONBLOCK
	o
}

impl<T> Fd<T> {
	pub fn from_fd(fd: RawFd, inner: T) -> Self {
		Fd {
			fd: fd,
			inner: inner,
		}
	}

	pub fn into_inner(self) -> T {
		self.inner
	}
}

impl<T: AsRawFd> Fd<T> {
	pub fn new(inner: T) -> Self {
		Fd {
			fd: inner.as_raw_fd(),
			inner: inner,
		}
	}
}

impl<T> Evented for Fd<T> {
	fn register(&self, poll: &mio::Poll, token: mio::Token, interest: mio::Ready, opts: mio::PollOpt) -> io::Result<()> {
		EventedFd(&self.fd).register(poll, token, interest, opts)
	}

	fn reregister(&self, poll: &mio::Poll, token: mio::Token, interest: mio::Ready, opts: mio::PollOpt) -> io::Result<()> {
		EventedFd(&self.fd).reregister(poll, token, interest, opts)
	}

	fn deregister(&self, poll: &mio::Poll) -> io::Result<()> {
		EventedFd(&self.fd).deregister(poll)
	}
}

impl<T: Read> Read for Fd<T> {
	fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
		self.inner.read(buf)
	}
}

impl<T: Write> Write for Fd<T> {
	fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
		self.inner.write(buf)
	}

	fn flush(&mut self) -> io::Result<()> {
		self.inner.flush()
	}
}
