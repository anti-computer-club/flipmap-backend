//! Implements a simple fixed-window limiter [RateLimit] intended for thread-safe operation in the
//! Tokio runtime. Spawns an internal task to reset. Lock-free.

use arc_swap::ArcSwap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use tokio::task::JoinHandle;
use tokio::time::{interval, Duration, Instant};
use tracing::instrument;

/// Implements a simple fixed-window rate limit
#[derive(Debug)]
pub struct RateLimit {
    /// Solely for logging
    name: String,
    // We don't need to track this after construction but we currently use it to give ctxt to trace
    reset_interval: Duration,
    /// How many requests may be made in a given fixed window
    limit: u32,
    /// How many have been made so far
    counter: Arc<AtomicU32>,
    // The tiny possibility of stale data influencing a response is no big deal here
    /// When the current window is expected to reset
    next_reset: Arc<ArcSwap<Instant>>,
    task_handle: JoinHandle<()>,
}

impl RateLimit {
    pub fn new(limit: u32, reset_interval: Duration, name: String) -> Self {
        let counter = Arc::new(AtomicU32::new(0));

        let next_reset = Arc::new(ArcSwap::new(Arc::new(Instant::now() + reset_interval)));

        let task_handle = tokio::spawn(RateLimit::reset_task(
            counter.clone(),
            next_reset.clone(),
            reset_interval,
            name.clone(),
        ));

        RateLimit {
            name,
            reset_interval,
            limit,
            counter,
            next_reset,
            task_handle,
        }
    }

    /// Attempts to consume `n` from the rate limit.
    ///
    /// Returns: `Ok(())` if it is possible, `Err(Instant)` otherwise, where `Instant`
    /// is the approximate time the window will reset next.
    ///
    /// Ok returns also increment internal counter. Atomic.
    pub fn try_consume(&self, n: u32) -> Result<(), Instant> {
        // Obvious answers for try-consuming 0 or more than is possible
        if n == 0 {
            return Ok(());
        }
        if n > self.limit {
            // This isn't a great API because reset doesn't matter here
            tracing::warn!("{n} tokens requested from ratelimiter '{}' which is more than will ever be available - max {} in per window",
                self.name, self.limit);
            return Err(*self.next_reset.load_full());
        }

        // We must retry each time another thread modifies the counter first
        // our information is no longer current in that case
        loop {
            //TODO: Audit ordering
            let count = self.counter.load(Ordering::Acquire);
            let new = count.saturating_add(n);

            // We would exceed the limit
            if new > self.limit {
                // Return the stored reset time on failure
                return Err(*self.next_reset.load_full());
            }

            match self
                .counter
                //TODO: Audit ordering
                .compare_exchange(count, new, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return Ok(()), // Success
                Err(_) => continue,     // Contention, retry loop
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
                    tracing::warn!("{:?}: rolling back ratelimit by {n}. this may cause usage underestimation if the limit was consumed in a prior window", self.name);
                    return;
                }
                Err(_) => continue,
            }
        }
    }

    /// Spawned in [RateLimit::new] to act as a timer which resets the limit and updates the
    /// next expected reset time.
    ///
    /// Makes logic a bit simpler and may cut down on contention vs if we try to spin
    /// for resets when checking in [RateLimit::try_consume]
    #[instrument(skip(next_reset))]
    async fn reset_task(
        counter: Arc<AtomicU32>,
        next_reset: Arc<ArcSwap<Instant>>,
        reset_interval: Duration,
        name: String,
    ) {
        let mut interval = interval(reset_interval);
        tracing::debug!(
            "{:?}: ratelimit reset task with interval {:?} now ticking",
            name,
            interval.period()
        );

        // First tick is immediate, consume it. We already set the first reset time in `new`.
        interval.tick().await;

        loop {
            // Calculate the *next* reset time *before* waiting for the tick.
            // This is redundant on first-run but more accurate. Lesser evil?
            let next_reset_time = Instant::now() + reset_interval;
            next_reset.store(Arc::new(next_reset_time));

            interval.tick().await;

            // Reset the counter for the *new* window that just started.
            // Relaxed is likely fine as the timing is primarily controlled by the interval timer.
            counter.store(0, Ordering::Relaxed);
            tracing::debug!(
                "{:?}: reset ratelimit counter, next reset in {:?}",
                name,
                reset_interval
            );
        }
    }
}

impl Drop for RateLimit {
    #[instrument()]
    fn drop(&mut self) {
        // TODO: See how instrumentation looks in practice
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
    /// any limit.
    ///
    /// Returns `Ok(())` on success, or `Err(Instant)` with the reset time of the *first* limit
    /// that failed.
    pub fn try_consume(&self, n: u32) -> Result<(), Instant> {
        let mut last_acceptor_idx = 0; // Track index up to which limits succeeded

        for (i, limit) in self.limits.iter().enumerate() {
            match limit.try_consume(n) {
                Ok(_) => {
                    // Only update if this limit succeeded
                    last_acceptor_idx = i + 1; // Store index *after* the successful one
                }
                Err(instant) => {
                    // Failure: undo consumption for all *previously successful* limits
                    // Use the stored index to slice correctly.
                    self.limits[..last_acceptor_idx]
                        .iter()
                        .for_each(|succeeded_limit| succeeded_limit.undo(n));
                    // Return the Instant from the limit that failed
                    return Err(instant);
                }
            }
        }
        // All limits succeeded
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::{timey_wime_check, SHORT_WAIT};
    use tokio::{task, time};

    /// Basic operation of a [RateLimit]: can we use all (and no further), but then use again after
    /// the refresh period has passed?
    #[tokio::test(start_paused = true)]
    async fn exhaust_and_refresh() {
        let limit = RateLimit::new(5, SHORT_WAIT, "Test!".to_string());

        let start_time = Instant::now();
        let expected_reset = start_time + SHORT_WAIT;

        // Exhaust limit
        for _ in 0..5 {
            assert!(limit.try_consume(1).is_ok());
        }
        // Next one should fail and return the expected reset time
        match limit.try_consume(1) {
            Ok(_) => panic!("Limit should have been exhausted"),
            Err(reset_time) => {
                assert!(timey_wime_check(reset_time, expected_reset));
            }
        }

        // Advance time instantly. The fact that both yields is required
        // is something of an incantation for me. I wish I knew what was going on.
        // Remove one and see. I dare you.
        task::yield_now().await;
        time::advance(SHORT_WAIT).await;
        task::yield_now().await;
        time::resume();

        // Verify we've reset time
        assert!(limit.try_consume(1).is_ok());
    }

    /// Ditto but with [LimitChain]
    #[tokio::test(start_paused = true)]
    async fn chain_exhaust_and_refresh() {
        let start_time = Instant::now();
        let expected_reset = start_time + SHORT_WAIT;
        let limits = [
            RateLimit::new(5, SHORT_WAIT, "Test!".to_string()),
            RateLimit::new(3, SHORT_WAIT, "Test2!".to_string()),
        ];
        let chain = LimitChain::new_from(&limits);

        // Exhaust limit of the second (stricter) limit
        assert!(chain.try_consume(2).is_ok());
        assert!(chain.try_consume(1).is_ok());
        // Ensure one is all it takes - should fail on the second limit
        match chain.try_consume(1) {
            Ok(_) => panic!("Chain limit should have been exhausted by the second limit"),
            Err(reset_time) => {
                // The reset time should come from the second limit (index 1)
                assert!(timey_wime_check(reset_time, expected_reset));

                // Both should be at 3. 1st is temporarily at 4 and then rolled back.
                assert_eq!(
                    limits[0].counter.load(Ordering::Relaxed),
                    3,
                    "First limit counter should be rolled back"
                );
                assert_eq!(
                    limits[1].counter.load(Ordering::Relaxed),
                    3,
                    "Second limit counter should be at its max"
                );
            }
        }

        task::yield_now().await;
        time::advance(SHORT_WAIT).await;
        task::yield_now().await;
        time::resume();

        // Verify we've reset time - both limits should allow consumption now
        assert!(chain.try_consume(1).is_ok());
        assert!(chain.try_consume(2).is_ok());
        // Again and again - should fail on the second limit again
        assert!(chain.try_consume(1).is_err());
    }

    /// Can we consume more than one from the [RateLimit] quota at once?
    #[tokio::test()]
    async fn exhaust_multiple() {
        let limit = RateLimit::new(5, SHORT_WAIT, "Test!".to_string());
        assert!(limit.try_consume(3).is_ok());
        assert!(limit.try_consume(2).is_ok());
        assert!(limit.try_consume(1).is_err()); // Should fail now
    }

    /// I prompted this so I'll just keep it. We've got a serious problem if it breaks
    #[tokio::test()]
    async fn test_zero_consumption() {
        let limit = RateLimit::new(5, SHORT_WAIT, "Test!".to_string());
        assert!(limit.try_consume(0).is_ok()); // Should always succeed with Ok(())
    }
}
