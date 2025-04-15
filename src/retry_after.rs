//! Implements lock-free state keeping for when to allow the next request after an HTTP 503 or 429
//! response. Uses supplied time from Retry-After, or a TBD backoff algorithm otherwise

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use arc_swap::ArcSwapOption;
use chrono::DateTime;
use tracing::instrument;

// TODO: please find a better name
#[derive(Debug, Default)]
pub struct BackerOff {
    /// Solely for logging
    name: Option<String>,
    //Note: <T> here is actually Arc<T> :think:
    until: ArcSwapOption<Instant>,
}

impl BackerOff {
    /// Creates a new `BackerOff` instance with no name and no initial backoff period.
    pub fn new() -> Self {
        BackerOff {
            name: None,
            until: ArcSwapOption::new(None),
        }
    }

    /// Sets an optional name for this backoff instance, for logging.
    pub fn with_name(mut self, name: String) -> Self {
        self.name = Some(name);
        self
    }

    /// Parses the value of a `Retry-After` header's value into an `Instant`.
    /// The caller is responsible for ensuring the passed &str is from X-Retry-After or Retry-After
    ///
    /// Handles both seconds-delay and HTTP-date formats. Returns `None` if parsing fails
    /// or the value represents a time in the past.
    #[instrument()]
    pub fn parse_retry_value(value: &str) -> Option<Instant> {
        if let Ok(secs) = value.parse::<u64>() {
            return Some(Instant::now() + Duration::from_secs(secs));
        }
        if let Ok(datetime) = DateTime::parse_from_rfc2822(value) {
            // We have a datatime, but no guarantee if it's in the future!
            // we need to check if this has passed according to our local system
            // If not, we must find how far in the future it is and use that to construct and return an
            // instant
            return None; //TODO:placeholder
        }
        tracing::warn!("couldn't parse provided str {value} into any HTTP-date");
        None
    }

    /// Stores the calculated `Instant` until which requests should be blocked.
    #[instrument(fields(name = self.name))]
    pub fn set_retry_until(&self, instant: Instant) {
        tracing::info!(
            "setting backoff until {:?}",
            instant.duration_since(Instant::now()) //TODO: is this right
        );
        self.until.store(Some(Arc::new(instant)));
    }

    /// Checks if a request is allowed based on the stored backoff time.
    ///
    /// Returns `true` if no backoff is active or if the backoff period has elapsed.
    /// Returns `false` if a backoff period is active and has not yet passed.
    ///
    /// If the backoff period has just elapsed, this method also clears the stored `Instant`.
    pub fn can_request(&self) -> bool {
        let guard = self.until.load();
        match *guard {
            None => true, // No backoff active
            Some(ref until_instant) => {
                let now = Instant::now();
                if now >= **until_instant {
                    // Backoff period has passed. Try to clear it.
                    // Another thread may have already done this, or set a new backoff period

                    // Might be cool to debug and see which thread tried vs succeeded in swapping,
                    // but not totally trivial to distinguish and log
                    let _ = self.until.compare_and_swap(&guard, None); // Attempt to clear
                    true
                } else {
                    // Backoff period still active
                    false
                }
            }
        }
    }
}
