//! Shared progress + cancellation, polled by the CLI's status line and the GUI.

use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};

#[derive(Default)]
pub struct Progress {
    total: AtomicU64,
    done: AtomicU64,
    cancel: AtomicBool,
    current: Mutex<String>,
}

/// Returned (inside anyhow) when the user cancels; callers can `downcast_ref` it.
#[derive(Debug)]
pub struct Cancelled;

impl fmt::Display for Cancelled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("cancelled")
    }
}

impl std::error::Error for Cancelled {}

impl Progress {
    pub fn new() -> Arc<Progress> {
        Arc::new(Progress::default())
    }

    pub fn set_total(&self, n: u64) {
        self.total.store(n, Relaxed);
    }

    /// Starts a new item (e.g. the next archive of a batch); keeps the cancel flag.
    pub fn reset(&self) {
        self.total.store(0, Relaxed);
        self.done.store(0, Relaxed);
    }

    pub fn add(&self, n: u64) {
        self.done.fetch_add(n, Relaxed);
    }

    pub fn set_current(&self, name: &str) {
        if let Ok(mut c) = self.current.lock() {
            c.clear();
            c.push_str(name);
        }
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Relaxed)
    }

    pub fn check(&self) -> anyhow::Result<()> {
        if self.is_cancelled() {
            return Err(Cancelled.into());
        }
        Ok(())
    }

    /// (done, total, current item)
    pub fn snapshot(&self) -> (u64, u64, String) {
        let cur = self.current.lock().map(|c| c.clone()).unwrap_or_default();
        (self.done.load(Relaxed), self.total.load(Relaxed), cur)
    }
}

pub fn is_cancelled(e: &anyhow::Error) -> bool {
    e.downcast_ref::<Cancelled>().is_some()
}
