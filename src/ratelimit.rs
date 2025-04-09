//! Implements a simple fixed-window limiter [RateLimit] intended for thread-safe operation in the
//! Tokio runtime. Spawns an internal task to reset.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinHandle;
use tokio::time::interval;

/// Implements a simple fixed-window rate limit
#[derive(Debug)]
pub struct RateLimit {
    reset_interval: Duration,
    limit: u32,
    counter: Arc<AtomicU32>,
    task_handle: JoinHandle<()>,
}

impl RateLimit {
    pub fn new(limit: u32, reset_interval: Duration) -> Self {
        let counter = Arc::new(AtomicU32::new(0));
        let counter_ref = counter.clone(); // Need another ref for task

        // Makes logic to reset simpler. Cuts down on possible contention vs if we try to do in
        // try_consume
        let task_handle = tokio::spawn(async move {
            let mut interval = interval(reset_interval);
            tracing::debug!(
                "ratelimit with interval {:?} now ticking",
                interval.period()
            );
            interval.tick().await; // First one's instant, so do it now.
            loop {
                interval.tick().await;
                //TODO: Audit ordering (this one's probably good)
                counter_ref.store(0, Ordering::Relaxed);
                //TODO: This won't be pretty and we might also benefit from instrumentation
                tracing::debug!("reset ratelimit with interval {:?}", interval.period())
            }
        });

        RateLimit {
            reset_interval,
            limit,
            counter,
            task_handle,
        }
    }

    /// Attempts to consume `n` from the rate limit.
    ///
    /// Returns: `true` if it is possible, `false` otherwise.
    ///
    /// True returns also increment internal counter. Atomic.
    pub fn try_consume(&self, n: u32) -> bool {
        // Obvious answers for try-consuming 0 or more than is possible
        if n == 0 {
            return true;
        }
        if n > self.limit {
            return false;
        }

        // We must retry each time another thread modifies the counter first
        // our information is no longer current in that case
        loop {
            //TODO: Audit ordering
            let count = self.counter.load(Ordering::Acquire);
            let new = count.saturating_add(n);

            // We would exceed the limit
            if new > self.limit {
                return false;
            }

            match self
                .counter
                //TODO: Audit ordering
                .compare_exchange(count, new, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return true,
                Err(_) => continue,
            }
        }
    }
}

impl Drop for RateLimit {
    fn drop(&mut self) {
        //TODO: Also yikes
        tracing::trace!(
            "aborting reset task for ratelimit with interval {:?}",
            self.reset_interval
        );
        self.task_handle.abort();
    }
}
