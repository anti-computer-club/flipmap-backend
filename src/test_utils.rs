//! Functions used in unit tests across modules.
use tokio::time::{Duration, Instant};

/// They say that monotonic clocks are monotonic. Duh. I say: why do two calls in my test code jump
/// back hundreds of nanoseconds?
///
/// This function checks if two instants are equal *enough*
pub fn timey_wime_check(a: Instant, b: Instant) -> bool {
    // Not cool: In isolation, we just need a ~1000ns factor to compensate for Rust trying to
    // take a monotonic clock read with higher res than is actually available causing jump back
    //
    // However, the time between these two instants being taken can be delayed further by noisy neighboring
    // tests. How much further? Hopefully no more than 50ms.
    const WIBBLE_FACTOR: Duration = Duration::from_millis(50);
    let before = b - WIBBLE_FACTOR;
    let after = b + WIBBLE_FACTOR;
    a > before && a < after
}
