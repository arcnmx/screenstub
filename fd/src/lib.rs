use std::os::unix::io::{AsRawFd, RawFd};

pub type FdRef<'a, T> = Fd<&'a T>;

#[derive(Copy, Clone, Debug)]
pub struct Fd<T = RawFd>(pub T);

impl<'a, T: AsRawFd> AsRawFd for Fd<&'a T> {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

impl AsRawFd for Fd<RawFd> {
    fn as_raw_fd(&self) -> RawFd {
        self.0
    }
}

impl<T> From<T> for Fd<T> {
    fn from(fd: T) -> Self {
        Self(fd)
    }
}
