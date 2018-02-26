use std::process::ExitStatus;
use std::{io, sync};
use futures::sync::mpsc;
use futures::unsync::mpsc as un_mpsc;
use {xcb, ddcutil};

quick_error! {
    #[derive(Debug)]
    pub enum Error {
        Generic(err: xcb::GenericError) {
            from()
            cause(err)
            display("Generic error: {}", err)
        }
        Conn(err: xcb::ConnError) {
            from()
            cause(err)
            display("Connection error: {}", err)
        }
        IO(err: io::Error) {
            from()
            cause(err)
            display("IO error: {}", err)
        }
        Ddc(err: ddcutil::Error) {
            from()
            cause(err)
            display("DDC error: {}", err)
        }
        MutexPoisoned(err: String) {
            display("Mutex poisoned: {}", err)
        }
        DdcNotFound {
            display("DDC monitor not found")
        }
        Exit {
            display("Event loop exited")
        }
    }
}

impl<T> From<mpsc::SendError<T>> for Error {
    fn from(e: mpsc::SendError<T>) -> Self {
        Error::from(io::Error::new(io::ErrorKind::Other, e.to_string()))
    }
}

impl<T> From<un_mpsc::SendError<T>> for Error {
    fn from(e: un_mpsc::SendError<T>) -> Self {
        Error::from(io::Error::new(io::ErrorKind::Other, e.to_string()))
    }
}

impl<T> From<sync::PoisonError<T>> for Error {
    fn from(e: sync::PoisonError<T>) -> Self {
        Error::MutexPoisoned(format!("{:?}", e))
    }
}

impl Error {
    pub fn from_exit_status(e: ExitStatus) -> Result<(), Self> {
        if e.success() {
            Ok(())
        } else {
            Err(io::Error::new(io::ErrorKind::Other,
                if let Some(code) = e.code() {
                    format!("process exited with code {}", code)
                } else {
                    "process exited with a failure".into()
                }
            ).into())
        }
    }
}
