//! Julian dates, sidereal time, and the local clock both the sky and the
//! fisherman run on.
//!
//! ## There is no timezone here, and that is a decision
//!
//! `std` has no local time and this workspace has no date crate, so "local
//! time" means **local mean solar time at the configured longitude** — noon is
//! when the sun is on the meridian, not when a government says it is.
//!
//! That is not a workaround. The sky is drawn for a place, and solar time is
//! the clock that place actually keeps; a sundial and a fisherman would both
//! use it. It also costs no dependency and, unlike a timezone database, every
//! line of it can be tested against a published number.
//!
//! The consequence worth knowing: with the default location, the fisherman goes
//! to lunch at San Francisco's solar noon regardless of where the machine is.
//! Set the location and he follows you.

/// Julian date of the Unix epoch, 1970-01-01 00:00:00 UTC.
pub const UNIX_EPOCH_JD: f64 = 2_440_587.5;
/// Julian date of J2000.0, 2000-01-01 12:00:00 TT — the epoch every formula
/// below counts from.
pub const J2000: f64 = 2_451_545.0;

/// Days in a Julian century, which is what the sidereal series is expanded in.
const DAYS_PER_CENTURY: f64 = 36_525.0;

/// Julian date from a Unix timestamp in seconds.
///
/// The whole clock enters the crate through here, so a caller passing a fixed
/// number gets a fixed sky — which is what makes every test below possible.
pub fn julian_date_from_unix(seconds: f64) -> f64 {
    seconds / 86_400.0 + UNIX_EPOCH_JD
}

/// Julian date from a UTC calendar date and time, by the Gregorian rule.
///
/// Present for tests and for saying what an instant *is* in the terms the
/// published values use.
pub fn julian_date(year: i32, month: u32, day: u32, hour: u32, minute: u32, second: f64) -> f64 {
    // January and February count as months 13 and 14 of the previous year, so
    // that the leap day lands at the end and `30.6001 * (m + 1)` stays a valid
    // month-length accumulator.
    let (y, m) = if month <= 2 {
        (year - 1, month + 12)
    } else {
        (year, month)
    };
    let a = (y as f64 / 100.0).floor();
    let b = 2.0 - a + (a / 4.0).floor();
    let day_fraction = day as f64 + hour as f64 / 24.0 + minute as f64 / 1440.0 + second / 86_400.0;

    (365.25 * (y as f64 + 4716.0)).floor() + (30.6001 * (m as f64 + 1.0)).floor() + day_fraction + b
        - 1524.5
}

/// Greenwich mean sidereal time, in degrees.
///
/// Meeus, *Astronomical Algorithms*, eq. 12.4. The leading coefficient is the
/// sidereal time at J2000.0 and the second is a sidereal day's worth of
/// rotation per solar day, which is why the sky drifts about four minutes
/// earlier each night.
pub fn greenwich_mean_sidereal_degrees(jd: f64) -> f64 {
    let d = jd - J2000;
    let t = d / DAYS_PER_CENTURY;
    let degrees =
        280.460_618_37 + 360.985_647_366_29 * d + 0.000_387_933 * t * t - t * t * t / 38_710_000.0;
    normalise_degrees(degrees)
}

/// Local mean sidereal time, in degrees. East longitude is positive.
///
/// This is the number that turns a star's fixed right ascension into an hour
/// angle, and so into something above or below *your* horizon.
pub fn local_sidereal_degrees(jd: f64, longitude_east_deg: f64) -> f64 {
    normalise_degrees(greenwich_mean_sidereal_degrees(jd) + longitude_east_deg)
}

/// Local mean solar time as hours since local midnight, in `0.0..24.0`.
///
/// The sun's own clock: the earth turns 15° an hour, so a place 15° east of
/// Greenwich sees noon an hour earlier.
pub fn local_solar_hours(jd: f64, longitude_east_deg: f64) -> f64 {
    // A Julian date rolls over at noon, not midnight, hence the half day.
    let utc_hours = (jd + 0.5).rem_euclid(1.0) * 24.0;
    (utc_hours + longitude_east_deg / 15.0).rem_euclid(24.0)
}

/// Whole days since J2000, keeping the fraction — the argument every orbital
/// series in this crate is expanded in.
pub fn days_since_j2000(jd: f64) -> f64 {
    jd - J2000
}

/// Fold an angle into `0.0..360.0`.
pub fn normalise_degrees(degrees: f64) -> f64 {
    degrees.rem_euclid(360.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The epoch is defined as 2000 January 1.5 TT. If this is wrong every
    /// other number in the crate is wrong by the same amount and nothing else
    /// would show it.
    #[test]
    fn j2000_is_the_instant_it_is_defined_as() {
        assert!((julian_date(2000, 1, 1, 12, 0, 0.0) - J2000).abs() < 1e-9);
    }

    /// Pins the Gregorian branch and the half-day offset together against a
    /// number that is not J2000, so an error in one cannot hide in the other.
    #[test]
    fn the_unix_epoch_has_its_published_julian_date() {
        assert!((julian_date(1970, 1, 1, 0, 0, 0.0) - UNIX_EPOCH_JD).abs() < 1e-9);
        assert!((julian_date_from_unix(0.0) - UNIX_EPOCH_JD).abs() < 1e-9);
        // One day of seconds is one day of Julian date.
        assert!((julian_date_from_unix(86_400.0) - (UNIX_EPOCH_JD + 1.0)).abs() < 1e-9);
    }

    /// January and February are the months the calendar mangles, so a date on
    /// either side of a leap day is worth stating outright.
    #[test]
    fn a_leap_day_is_one_day_long() {
        let feb28 = julian_date(2024, 2, 28, 0, 0, 0.0);
        let feb29 = julian_date(2024, 2, 29, 0, 0, 0.0);
        let mar01 = julian_date(2024, 3, 1, 0, 0, 0.0);
        assert!((feb29 - feb28 - 1.0).abs() < 1e-9);
        assert!((mar01 - feb29 - 1.0).abs() < 1e-9);
    }

    /// Greenwich sidereal time at 0h UT on 2000 January 1 is a published value:
    /// 6h 39m 52.27s. Checking against an almanac rather than against the
    /// formula's own leading term is the point — the leading term alone would
    /// pass a test that never left the equation it came from.
    #[test]
    fn greenwich_sidereal_time_matches_the_published_value_for_2000_january_1() {
        let jd = julian_date(2000, 1, 1, 0, 0, 0.0);
        let hours = greenwich_mean_sidereal_degrees(jd) / 15.0;
        let published = 6.0 + 39.0 / 60.0 + 52.27 / 3600.0;
        assert!(
            (hours - published).abs() < 1.0 / 3600.0,
            "sidereal time {hours:.6} h, published {published:.6} h"
        );
    }

    /// A sidereal day is about four minutes short of a solar one. That gap is
    /// the whole reason the constellations move through the year, and getting
    /// its sign wrong runs the sky backwards.
    #[test]
    fn a_sidereal_day_is_shorter_than_a_solar_day() {
        let jd = julian_date(2024, 3, 15, 0, 0, 0.0);
        let today = greenwich_mean_sidereal_degrees(jd);
        let tomorrow = greenwich_mean_sidereal_degrees(jd + 1.0);
        let gained = (tomorrow - today).rem_euclid(360.0);
        // 24h of solar time is 24h 3m 56s of sidereal time: just under a degree.
        assert!(
            (gained - 0.9856).abs() < 0.001,
            "the sky gained {gained:.4}° in a day"
        );
    }

    /// Noon UTC at Greenwich is noon. Fifteen degrees east it is one o'clock,
    /// and the same distance west it is eleven — the sign of the longitude term
    /// is the easiest thing here to get backwards.
    #[test]
    fn solar_noon_follows_the_longitude_east() {
        let jd = julian_date(2024, 6, 1, 12, 0, 0.0);
        assert!((local_solar_hours(jd, 0.0) - 12.0).abs() < 1e-6);
        assert!((local_solar_hours(jd, 15.0) - 13.0).abs() < 1e-6);
        assert!((local_solar_hours(jd, -15.0) - 11.0).abs() < 1e-6);
    }

    /// Midnight is where the day wraps, and the Julian date's own rollover is
    /// at noon — two offsets that cancel, which is exactly the kind of thing
    /// that stays broken until something states it.
    #[test]
    fn the_solar_clock_wraps_at_midnight_not_at_noon() {
        let just_before = julian_date(2024, 6, 1, 23, 59, 0.0);
        let just_after = julian_date(2024, 6, 2, 0, 1, 0.0);
        assert!(local_solar_hours(just_before, 0.0) > 23.9);
        assert!(local_solar_hours(just_after, 0.0) < 0.1);
    }
}
