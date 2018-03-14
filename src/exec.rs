use std::ffi::OsStr;
use std::process::{Command, Stdio, ExitStatus};
use tokio_core::reactor::Handle;
use tokio_process::CommandExt;
use futures::{future, Future};
use failure::Error;

pub fn exec<I: IntoIterator<Item=S>, S: AsRef<OsStr>>(ex: &Handle, args: I) -> Box<Future<Item=(), Error=Error>> {
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

    let mut args = args.into_iter();
    if let Some(cmd) = args.next() {
        let child = Command::new(cmd)
            .args(args)
            .stdout(Stdio::null())
            .stdin(Stdio::null())
            .spawn_async(ex);
        Box::new(future::result(child)
            .and_then(|c| c).map_err(Error::from)
            .and_then(exit_status_error)
        ) as Box<_>
    } else {
        Box::new(future::err(format_err!("Missing exec command"))) as Box<_>
    }
}
