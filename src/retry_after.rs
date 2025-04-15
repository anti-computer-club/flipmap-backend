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

    /// Parses the value of a `Retry-After` header and blocks further requests until time, if it's
    /// in the future.
    ///
    /// The caller is responsible for ensuring the passed &str is from X-Retry-After or Retry-After
    /// Handles both seconds-delay and HTTP-date formats as per RFC9110
    ///
    /// Returns `None` if parsing fails or the value represents a time in the past.
    /// Returns `Some(Instant)` if a future instant was set
    pub fn parse_maybe_set(&self, value: &str) -> Option<Instant> {
        //TODO: Consider if this is a bad API... later
        if let Some(monotonically_later) = self.parse_retry_value(value) {
            self.set_retry_until(monotonically_later);
            Some(monotonically_later)
        } else {
            None
        }
    }

    /// For when we get a response we'd want to block further requests for, but don't know until how long.
    ///
    /// Ideally, would use some exponential backoff, but that'd take some wacky state-keeping inside
    /// so currently it's just a 30s pause.
    pub fn maybe_set_without_header(&self) -> Option<Instant> {
        let later = Instant::now() + Duration::from_secs(30);
        self.set_retry_until(later);
        Some(later)
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

    /// Stores the calculated `Instant` until which requests should be blocked.
    #[instrument(fields(name = self.name))]
    fn set_retry_until(&self, instant: Instant) {
        tracing::info!(
            "setting backoff until {:?}",
            instant.duration_since(Instant::now()) //TODO: is this right
        );
        self.until.store(Some(Arc::new(instant)));
    }

    #[instrument()]
    fn parse_retry_value(&self, value: &str) -> Option<Instant> {
        if let Ok(secs) = value.parse::<u64>() {
            return Some(Instant::now() + Duration::from_secs(secs));
        }
        if let Ok(datetime) = DateTime::parse_from_rfc2822(value) {
            // We have a datetime, but no guarantee if it's in the future!
            // We need to check if this has passed according to our local system time.
            let now_utc = chrono::Utc::now();
            let datetime_utc = datetime.with_timezone(&chrono::Utc);

            if datetime_utc > now_utc {
                // It's in the future. Calculate the duration.
                // This duration conversion should be safe as we've checked it's positive.
                match (datetime_utc - now_utc).to_std() {
                    Ok(duration) => return Some(Instant::now() + duration),
                    Err(e) => {
                        // This case (negative duration) should technically be impossible
                        // due to the `datetime_utc > now_utc` check, but handle defensively.
                        tracing::error!(
                            "unexpected negative time delta during HTTP-time parsing: {e:?}"
                        );
                        return None;
                    }
                }
            } else {
                // The specified time is in the past, so no backoff needed from this header.
                tracing::debug!("parsed HTTP-date {value} is in the past, ignoring");
                return None;
            }
        }
        tracing::warn!("couldn't parse provided str {value} into seconds or HTTP-date");
        None
    }
}
