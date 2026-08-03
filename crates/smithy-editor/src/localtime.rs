//! The system's own idea of what time it is.
//!
//! `smithy-sky` deliberately has no dependencies, so it speaks UTC and a
//! location and nothing else. This is the one place that asks the operating
//! system what the local offset is, and it lives here rather than there for
//! that reason.
//!
//! ## Why not a date crate
//!
//! `libc` is already in the dependency tree, and `localtime_r` gives the whole
//! answer — `tm_gmtoff` is the seconds east of UTC *including* whatever daylight
//! saving is in force at that instant, resolved by the platform's own timezone
//! database. A date crate would pull in a second copy of that machinery to
//! answer a question the C library already answers.
//!
//! The first version of this had no system time at all: local time was solar
//! time at a hardcoded San Francisco, so the fisherman went to lunch at *San
//! Francisco's* noon wherever the machine was. That is defensible for the sky,
//! which is drawn for a place, and indefensible for a clock.

/// Seconds east of UTC right now, as the system reckons it.
///
/// Daylight saving included, because `tm_gmtoff` is the offset actually in
/// force at the instant asked about rather than the zone's standard offset.
///
/// Zero if the platform will not say — a backdrop is not a reason to fail, and
/// the consequence of being wrong is that an ornament keeps Greenwich's hours.
pub fn utc_offset_seconds() -> i64 {
    // SAFETY: `time` accepts a null pointer and returns the current time;
    // `localtime_r` writes into a `tm` we own and borrow exclusively. Both are
    // documented to be safe with these arguments, and `localtime_r` is the
    // reentrant form specifically so it can be called from any thread.
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut local: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&now, &mut local).is_null() {
            return 0;
        }
        local.tm_gmtoff as i64
    }
}

/// Hours east of UTC right now.
pub fn utc_offset_hours() -> f64 {
    utc_offset_seconds() as f64 / 3600.0
}

/// The local wall-clock time, in hours since midnight.
///
/// This is the clock the fisherman keeps: *your* noon, not the sun's.
pub fn local_hours(unix_seconds: f64) -> f64 {
    (unix_seconds / 3600.0 + utc_offset_hours()).rem_euclid(24.0)
}

/// Which local day it is, as days since the Unix epoch.
///
/// Rolls over at local midnight, so a per-day seed does not change its mind
/// halfway through somebody's afternoon.
pub fn local_day(unix_seconds: f64) -> i64 {
    ((unix_seconds + utc_offset_seconds() as f64) / 86_400.0).floor() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whatever the machine's zone is, the offset has to be a real one. A
    /// failure here means `tm_gmtoff` was misread — a units mistake would put
    /// it in the thousands.
    #[test]
    fn the_system_offset_is_a_real_timezone_offset() {
        let hours = utc_offset_hours();
        assert!(
            (-12.0..=14.0).contains(&hours),
            "{hours} hours from UTC is not a timezone anyone lives in"
        );
        // Every real zone is a whole number of quarter hours.
        let quarters = hours * 4.0;
        assert!(
            (quarters - quarters.round()).abs() < 1e-6,
            "{hours} is not a whole quarter of an hour"
        );
    }

    /// The clock has to advance with the seconds it is given, and wrap at
    /// midnight rather than at some other hour.
    #[test]
    fn the_local_clock_advances_and_wraps_at_midnight() {
        let base = 1_700_000_000.0;
        let now = local_hours(base);
        assert!((0.0..24.0).contains(&now));

        let hour_later = local_hours(base + 3600.0);
        assert!(
            ((hour_later - now - 1.0).abs() < 1e-9) || ((hour_later - now + 23.0).abs() < 1e-9),
            "an hour later read {hour_later:.4} against {now:.4}"
        );

        // A whole day later is the same time of day, and one day on.
        assert!((local_hours(base + 86_400.0) - now).abs() < 1e-9);
        assert_eq!(local_day(base + 86_400.0), local_day(base) + 1);
    }

    /// The day must turn over exactly when the clock does, or the fisherman
    /// picks a new lunch time in the middle of an afternoon.
    #[test]
    fn the_local_day_turns_over_when_the_local_clock_wraps() {
        let base = 1_700_000_000.0;
        let mut rollovers = 0;
        for minute in 1..(2 * 24 * 60) {
            let now = base + f64::from(minute) * 60.0;
            let before = now - 60.0;
            let wrapped = local_hours(now) < local_hours(before);
            let day_changed = local_day(now) != local_day(before);
            assert_eq!(wrapped, day_changed, "at minute {minute}");
            rollovers += i32::from(day_changed);
        }
        assert_eq!(rollovers, 2, "two days hold two midnights");
    }
}
