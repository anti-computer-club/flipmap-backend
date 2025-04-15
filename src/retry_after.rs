//! Implements lock-free state keeping for when to allow the next request after an HTTP 503 or 429
//! response. Uses supplied time from Retry-After, or a TBD backoff algorithm otherwise

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use arc_swap::ArcSwapOption;
use chrono::DateTime;
#[cfg(not(test))]
use chrono::Utc;
use tracing::instrument;

// TODO: please find a better name im begging you (me)
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
        if let Some(delay) = self.parse_retry_value(value) {
            let monotonically_later = Instant::now() + delay;
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

    /// Stores the calculated `Instant` until which requests should be blocked
    #[instrument(fields(name = self.name))]
    fn set_retry_until(&self, instant: Instant) {
        //TODO: Consider whether we could end up over-writing a pre-existing instant in a
        //problematic way
        tracing::info!(
            "setting backoff until {:?}",
            instant.duration_since(Instant::now()) //TODO: is this right
        );
        self.until.store(Some(Arc::new(instant)));
    }

    #[instrument()]
    fn parse_retry_value(&self, value: &str) -> Option<Duration> {
        if let Ok(secs) = value.parse::<u64>() {
            return Some(Duration::from_secs(secs));
        }
        if let Ok(datetime) = DateTime::parse_from_rfc2822(value) {
            // We have a datetime, but no guarantee if it's in the future!
            // We need to check if this has passed according to our local system time.
            let now_utc = Utc::now();
            let datetime_utc = datetime.with_timezone(&chrono::Utc);

            if datetime_utc > now_utc {
                // It's in the future. Calculate the duration.
                // This duration conversion should be safe as we've checked it's positive.
                match (datetime_utc - now_utc).to_std() {
                    Ok(duration) => return Some(duration),
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

#[cfg(test)]
use utc_mock::Utc;

#[cfg(test)]
mod utc_mock {
    use chrono::{DateTime, NaiveDate, TimeZone};
    pub struct Utc {}
    impl Utc {
        pub fn now() -> DateTime<chrono::Utc> {
            let dt = NaiveDate::from_ymd_opt(2001, 1, 1)
                .unwrap()
                .and_hms_opt(9, 30, 0)
                .unwrap();
            chrono::Utc.from_utc_datetime(&dt)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_mocked_utc() {
        let now = Utc::now();
        // theoretically this could fail with an update to chrono because this standard isn't
        // toally prescriptive
        assert_eq!(now.to_rfc2822(), "Mon, 1 Jan 2001 09:30:00 +0000");
    }

    // Any non-negative decimal integer should be parsable to seconds according to RFC9110 10.2.3
    #[test]
    fn parse_retry_value_seconds_60() {
        let backer = BackerOff::new();
        assert_eq!(
            backer.parse_retry_value("60"),
            Some(Duration::from_secs(60))
        );
    }

    #[test]
    fn parse_retry_value_seconds_120() {
        let backer = BackerOff::new();
        assert_eq!(
            backer.parse_retry_value("120"),
            Some(Duration::from_secs(120))
        );
    }

    #[test]
    fn parse_retry_value_seconds_0() {
        let backer = BackerOff::new();
        assert_eq!(backer.parse_retry_value("0"), Some(Duration::from_secs(0)));
    }

    #[test]
    fn parse_retry_value_seconds_u64_max() {
        let backer = BackerOff::new();
        let max_str = u64::MAX.to_string();
        assert_eq!(
            backer.parse_retry_value(&max_str),
            Some(Duration::from_secs(u64::MAX))
        );
    }

    // So, not this one
    #[test]
    fn parse_retry_value_seconds_negative() {
        let backer = BackerOff::new();
        // u64::from_str will fail, resulting in None
        assert_eq!(backer.parse_retry_value("-60"), None);
    }

    // IMF-fixdate is the preferred HTTP-date format according to RFC9110 5.6.7
    #[test]
    fn parse_retry_value_imf_future() {
        let backer = BackerOff::new();
        // Mock time is Mon, 1 Jan 2001 09:30:00 +0000
        // This is 1 hour (3600 seconds) after mock time
        let future_date = "Mon, 1 Jan 2001 10:30:00 +0000";
        assert_eq!(
            backer.parse_retry_value(future_date),
            Some(Duration::from_secs(3600))
        );
    }

    #[test]
    fn parse_retry_value_imf_past() {
        let backer = BackerOff::new();
        // Mock time is Mon, 1 Jan 2001 09:30:00 +0000
        // This is 1 hour before mock time
        let past_date = "Mon, 1 Jan 2001 08:30:00 +0000";
        assert_eq!(backer.parse_retry_value(past_date), None);
    }

    // RFC850 (Usenet!) format is one of two accepted obsolete types
    #[test]
    fn parse_retry_value_rfc850_future() {
        let backer = BackerOff::new();
        // Mock time is Mon, 1 Jan 2001 09:30:00 +0000
        // This is 1 hour (3600 seconds) after mock time
        let future_date = "Monday, 01-Jan-01 10:30:00 GMT";
        assert_eq!(
            backer.parse_retry_value(future_date),
            Some(Duration::from_secs(3600))
        );
    }

    #[test]
    fn parse_retry_value_rfc850_past() {
        let backer = BackerOff::new();
        // Mock time is Mon, 1 Jan 2001 09:30:00 +0000
        // This is 1 hour before mock time
        let past_date = "Monday, 01-Jan-01 08:30:00 GMT";
        assert_eq!(backer.parse_retry_value(past_date), None);
    }

    // `asctime` format is the other one
    #[test]
    fn parse_retry_value_asctime_future() {
        let backer = BackerOff::new();
        // Mock time is Mon Jan  1 09:30:00 2001
        // This is 1 hour (3600 seconds) after mock time
        let future_date = "Mon Jan  1 10:30:00 2001";
        assert_eq!(
            backer.parse_retry_value(future_date),
            Some(Duration::from_secs(3600))
        );
    }

    #[test]
    fn parse_retry_value_asctime_past() {
        let backer = BackerOff::new();
        // Mock time is Mon Jan  1 09:30:00 2001
        // This is 1 hour before mock time
        let past_date = "Mon Jan  1 08:30:00 2001";
        assert_eq!(backer.parse_retry_value(past_date), None);
    }
}
