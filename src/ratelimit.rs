//! Implements a simple fixed-window limiter [RateLimit] intended for thread-safe operation in the
//! Tokio runtime. Spawns an internal task to reset.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinHandle;
use tokio::time::{interval, pause};

/// Implements a simple fixed-window rate limit
#[derive(Debug)]
pub struct RateLimit {
    // We don't need to track this after construction but we currently use it to give ctxt to trace
    reset_interval: Duration,
    /// How many requests may be made in a given fixed window
    limit: u32,
    /// How many have been made so far
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

    /// Used by [LimitChain] when this limit returns true but ones after do not, so we must then
    /// 'undo' so that we do not act as if limits were used when the request was not actually sent
    ///
    /// The limit may reset in the time between initial check and later reset. If another request
    /// happens after the reset and is approved, we will 'undo' from the wrong window. This is O.K
    fn undo(&self, n: u32) {
        loop {
            //TODO: Audit ordering
            let count = self.counter.load(Ordering::Acquire);
            let new = count.saturating_sub(n);

            match self
                .counter
                //TODO: Audit ordering
                .compare_exchange(count, new, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => {
                    // This could theoretically happen quite often in a busy application. -> debug
                    // or lower if it gets annoying
                    tracing::warn!("rolling back ratelimit by {n}. this may cause usage underestimation if the limit was consumed in a prior window");
                    return;
                }
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

/// Allows multiple [RateLimit] to be used sequentially. Failure of any individual [RateLimit]
/// causes a false. Handles 'undoing' usage for all pevious [RateLimit] before a failure.
///
/// Undoing may cause undercounts of usage under some circumstances. There is no current attempt to
/// track or mediate these. See: [RateLimit::undo()]
///
/// It is also worth noting that refresh timers for [RateLimit] are independent, which means even
/// those with the same interval will not refresh at the same time.
#[derive(Debug)]
pub struct LimitChain<'a> {
    limits: Vec<&'a RateLimit>,
}

impl<'a> LimitChain<'a> {
    pub fn new_from(limits: &'a [RateLimit]) -> Self {
        LimitChain {
            limits: limits.iter().collect(),
        }
    }

    /// Attempt to consume n quota items from every included [RateLimit]. Undoes upon failure of
    /// any limit
    pub fn try_consume(&self, n: u32) -> bool {
        let mut last_acceptor = 0;
        for (i, limit) in self.limits.iter().enumerate() {
            if limit.try_consume(n) {
                last_acceptor = i;
            } else {
                // Any failure means we must go back to each previous limit and put the tokens back. This is imperfect.
                self.limits[..last_acceptor]
                    .iter()
                    .for_each(|limit| limit.undo(n));
                return false;
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::{
        task,
        time::{self, Duration},
    };

    /// Basic operation of a [RateLimit]: can we use all (and no further), but then use again after
    /// the refresh period has passed?
    #[tokio::test(start_paused = true)]
    async fn exhaust_and_refresh() {
        let limit = RateLimit::new(5, Duration::from_secs(1));

        // Exhaust limit
        for _ in 0..5 {
            assert!(limit.try_consume(1));
        }
        assert!(!limit.try_consume(1));

        // Advance time instantly. The fact that both yields is required
        // is something of an incantation for me. I wish I knew what was going on
        // Remove one and see. I dare you.
        task::yield_now().await;
        time::advance(Duration::from_secs(1)).await;
        task::yield_now().await;

        // Verify we've reset time
        assert!(limit.try_consume(1));
    }

    /// Ditto but with [LimitChain]
    #[tokio::test(start_paused = true)]
    async fn chain_exhaust_and_refresh() {
        let limits = [
            RateLimit::new(5, Duration::from_secs(1)),
            RateLimit::new(3, Duration::from_secs(1)),
        ];
        let chain = LimitChain::new_from(&limits);

        // Exhaust limit of one
        assert!(chain.try_consume(2));
        assert!(chain.try_consume(1));
        // Ensure one is all it takes
        assert!(!chain.try_consume(1));

        task::yield_now().await;
        time::advance(Duration::from_secs(1)).await;
        task::yield_now().await;

        // Verify we've reset time
        assert!(chain.try_consume(1));
        assert!(chain.try_consume(2));
        // Again and again
        assert!(!chain.try_consume(1));
    }

    /// Can we consume more than one from the [RateLimit] quota at once?
    #[tokio::test(start_paused = true)]
    async fn exhaust_multiple() {
        let limit = RateLimit::new(5, Duration::from_secs(1));

        assert!(limit.try_consume(3));
        assert!(limit.try_consume(2));
        assert!(!limit.try_consume(1));
    }

    /// I prompted this so I'll just keep it. We've got a serious problem if it breaks
    #[tokio::test(start_paused = true)]
    async fn test_zero_consumption() {
        let limit = RateLimit::new(5, Duration::from_secs(1));
        assert!(limit.try_consume(0)); // Should always succeed
    }
}
