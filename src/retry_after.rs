//! Implements lock-free state keeping for when to allow the next request after an HTTP 503 or 429
//! response. Uses supplied time from Retry-After, or a TBD backoff algorithm otherwise

use std::sync::Arc;

use crate::error::RouteError;
use arc_swap::ArcSwapOption;
use httpdate::parse_http_date;
use std::time::SystemTime;
use tokio::time::{Duration, Instant};
use tracing::instrument;

/// In lieu of a proper algorithm, we wait this long if the server sends a backoff worthy response
/// without a Retry-After header
const HEADERLESS_BACKOFF_TIME: Duration = Duration::from_secs(30);

#[derive(Debug, Default)]
pub struct BackerOff {
    /// Solely for logging
    name: Option<String>,
    //Note: <T> here is actually Arc<T> :think:
    until: ArcSwapOption<Instant>,
}

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("failed to parse input {0} to header")]
    ParseFail(String),
    #[error("parsed input represents a time already passed")]
    FromPast,
    // LaterSet, we don't (need to?) care if a later value is set already tbh
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
    /// Returns [Error] if parsing fails or the value represents a time in the past
    ///
    /// Returns Ok if a future instant was set
    pub fn parse_maybe_set(&self, value: &str) -> Result<(), Error> {
        let delay = self.parse_retry_value(value)?;
        let monotonically_later = Instant::now() + delay;
        self.set_retry_until(monotonically_later);
        Ok(())
    }

    /// For when we get a response we'd want to block further requests for, but don't know until how long.
    ///
    /// Ideally, would use some exponential backoff, but that'd take some wacky state-keeping inside
    /// so currently it's just a 30s pause.
    pub fn set_without_header(&self) {
        //TODO: Stateful backoff?
        let later = Instant::now() + HEADERLESS_BACKOFF_TIME;
        self.set_retry_until(later);
    }

    /// Checks if a request is allowed based on the stored backoff time.
    ///
    /// Returns `Ok(())` if no backoff is active or if the backoff period has elapsed.
    ///
    /// Returns [RouteError::ExternalAPILimit] if a backoff period is active
    ///
    /// If the backoff period has just elapsed, this method also clears the stored `Instant`.
    pub fn can_request(&self) -> Result<(), RouteError> {
        let guard = self.until.load();
        match *guard {
            None => Ok(()), // No backoff active
            Some(ref until_instant) => {
                let now = Instant::now();
                if now >= **until_instant {
                    // Backoff period has passed. Try to clear it.
                    // Another thread may have already done this, or set a new backoff period

                    // Might be cool to debug and see which thread tried vs succeeded in swapping,
                    // but not totally trivial to distinguish and log
                    let _ = self.until.compare_and_swap(&guard, None); // Attempt to clear
                    Ok(())
                } else {
                    // Backoff period still active
                    Err(RouteError::ExternalAPILimit(**until_instant))
                }
            }
        }
    }

    /// Returns a copy of Instant represents a possible future expiry of the retry-after, if any.
    pub fn get_retry_until(&self) -> Option<Instant> {
        Some(*self.until.load_full()?)
    }

    /// If Stores the calculated `Instant` until which requests should be blocked
    #[instrument(fields(name = self.name))]
    fn set_retry_until(&self, instant: Instant) {
        // Theoretically problematic: If the same endpoint gives us retry-after headers only on
        // some requests OR does not give us a monotonically decreasing retry-after we can
        // over-write in BAD ways
        //
        // We'll assume that doesn't happen regularly. A stray-cosmic ray isn't a show-stopper.
        tracing::info!(
            "setting backoff until {:?}",
            instant.duration_since(Instant::now())
        );
        self.until.store(Some(Arc::new(instant)));
    }

    #[instrument()]
    fn parse_retry_value(&self, value: &str) -> Result<Duration, Error> {
        if let Ok(secs) = value.parse::<u64>() {
            return Ok(Duration::from_secs(secs));
        }
        if let Ok(datetime) = parse_http_date(value) {
            // We have a datetime, but no guarantee if it's in the future!
            // We need to check if this has passed according to our local system time.
            let now = SystemTime::now();

            // Find out if it's from the future or not
            return match datetime.duration_since(now) {
                Ok(duration) => Ok(duration),
                Err(e) => {
                    //TODO: Are there other possible errors here? I think not
                    tracing::warn!(
                        "parsed HTTP-date {datetime:?} is in the past ({e:?}), ignoring"
                    );
                    Err(Error::FromPast)
                }
            };
        }
        tracing::warn!("couldn't parse provided str {value} into seconds or HTTP-date");
        Err(Error::ParseFail(value.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use httpdate::fmt_http_date;
    use tokio::time;

    use super::*;

    #[tokio::test(start_paused = true)]
    async fn block_without_header() {
        let backer = BackerOff::new();
        backer.set_without_header();
        dbg!(&backer, tokio::time::Instant::now());
        assert!(backer
            .can_request()
            .is_err_and(|x| matches!(x, RouteError::ExternalAPILimit(_))));
        time::advance(HEADERLESS_BACKOFF_TIME + Duration::from_millis(100)).await;
        assert!(backer.can_request().is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn block_with_int_header() {
        let backer = BackerOff::new();
        assert!(backer.parse_maybe_set("60").is_ok());
        assert!(backer
            .can_request()
            .is_err_and(|x| matches!(x, RouteError::ExternalAPILimit(_))));
        time::advance(Duration::from_secs(60)).await;
        assert!(backer.can_request().is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn block_with_httpdate_header() {
        let until = SystemTime::now() + Duration::from_secs(20);
        let str_until = fmt_http_date(until);

        let backer = BackerOff::new();
        assert!(backer.parse_maybe_set(str_until.as_str()).is_ok());
        assert!(backer
            .can_request()
            .is_err_and(|x| matches!(x, RouteError::ExternalAPILimit(_))));
        time::advance(Duration::from_secs(20)).await;
        assert!(backer.can_request().is_ok());
    }
}
