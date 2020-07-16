use std::ffi::OsStr;
use std::future::Future;
use std::process::{Stdio, ExitStatus};
use tokio::process::Command;
use failure::{Error, format_err};

pub struct Builder {
    child: Option<Command>,
}

impl Builder {
    pub fn into_future(self) -> impl Future<Output=Result<(), Error>> + Send + 'static {
        async move {
            if let Some(mut child) = self.child {
                exit_status_error(child.spawn()?.await?)
            } else {
                Err(format_err!("Missing exec command"))
            }
        }
    }
}

pub fn exec<I: IntoIterator<Item=S>, S: AsRef<OsStr>>(args: I) -> Builder {
    let mut args = args.into_iter();
    let child = if let Some(cmd) = args.next() {
        let mut child = Command::new(cmd);
        child
            .args(args)
            .stdout(Stdio::null())
            .stdin(Stdio::null());
        Some(child)
    } else {
        None
    };

    Builder {
        child,
    }
}

fn exit_status_error(status: ExitStatus) -> Result<(), Error> {
    if status.success() {
        Ok(())
    } else {
        Err(if let Some(code) = status.code() {
            format_err!("process exited with code {}", code)
        } else {
            format_err!("process exited with a failure")
        })
    }
}
