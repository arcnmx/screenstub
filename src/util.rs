use std::time::Duration;
use std::future::Future;
use std::hint;
use futures::{future, TryFutureExt};
use failure::Error;
use tokio::time;

pub fn retry<R, E: Into<Error>, F: Future<Output=Result<R, E>>, FF: FnMut() -> F>(mut f: FF, retries: usize, timeout: Duration) -> impl Future<Output=Result<R, Error>> {
    time::timeout(timeout, async move {
        let mut err = None;
        for _ in 0..=retries {
            match f().await {
                Ok(res) => return Ok(res),
                Err(e) => err = Some(e),
            }
        }
        match err {
            Some(err) => Err(err.into()),
            None => unsafe { hint::unreachable_unchecked() },
        }
    }).map_err(Error::from)
    .and_then(|r| future::ready(r))
}
