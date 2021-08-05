use std::sync::Mutex;
use tokio::time::{Duration, Instant, timeout_at};
use tokio::task::JoinHandle;
use futures::{Future, future};
use anyhow::Error;

pub struct Spawner {
    handles: Mutex<Vec<JoinHandle<()>>>,
}

impl Spawner {
    pub fn new() -> Self {
        Self {
            handles: Mutex::new(Vec::new()),
        }
    }

    pub fn spawn<F: Future<Output=()> + Send + 'static>(&self, f: F) {
        let handle = tokio::spawn(f);
        self.handles.lock().unwrap().push(handle);
    }

    pub async fn join_timeout(&self, timeout: Duration) -> Result<(), Error> {
        let deadline = Instant::now() + timeout;
        loop {
            let handles: Vec<_> = self.handles.lock().unwrap().drain(..).collect();
            if handles.is_empty() {
                break Ok(())
            }
            let _: Vec<()> = timeout_at(deadline, future::try_join_all(handles)).await??;
        }
    }
}
