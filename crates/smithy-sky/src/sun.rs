//! Where the sun is.
//!
//! The low-precision solar position from Meeus chapter 25, good to about 0.01°
//! for a couple of centuries either side of J2000. The sun is drawn as a disc
//! half a degree across on a backdrop, so this is roughly a thousand times more
//! accuracy than the picture can show — but it is also what decides the sky's
//! colour, and *that* is a boundary a person can notice being wrong by a minute.

use crate::coords::{ecliptic_to_equatorial, mean_obliquity_deg};
use crate::time::{days_since_j2000, normalise_degrees};
use crate::Equatorial;

/// The sun's apparent equatorial position at a Julian date.
pub fn position(jd: f64) -> Equatorial {
    let n = days_since_j2000(jd);

    // Mean longitude and mean anomaly: where the sun would be if the orbit
    // were a circle, and how far round that circle it has gone.
    let mean_longitude = normalise_degrees(280.460 + 0.985_647_4 * n);
    let mean_anomaly = normalise_degrees(357.528 + 0.985_600_3 * n).to_radians();

    // The equation of centre corrects the circle to the real ellipse. Two
    // terms is the whole of the low-precision method; the third is under an
    // arcsecond.
    let true_longitude =
        mean_longitude + 1.915 * mean_anomaly.sin() + 0.020 * (2.0 * mean_anomaly).sin();

    // The sun defines the ecliptic, so its ecliptic latitude is zero by
    // construction — that is what makes this the short formula.
    ecliptic_to_equatorial(
        normalise_degrees(true_longitude),
        0.0,
        mean_obliquity_deg(n),
    )
}

/// The sun's ecliptic longitude, in degrees.
///
/// Kept separate because the moon's phase needs the sun's position *along the
/// ecliptic*, and going out to equatorial coordinates and back to get it would
/// be arithmetic in a circle.
pub fn ecliptic_longitude_deg(jd: f64) -> f64 {
    let n = days_since_j2000(jd);
    let mean_longitude = 280.460 + 0.985_647_4 * n;
    let mean_anomaly = normalise_degrees(357.528 + 0.985_600_3 * n).to_radians();
    normalise_degrees(
        mean_longitude + 1.915 * mean_anomaly.sin() + 0.020 * (2.0 * mean_anomaly).sin(),
    )
}

/// The altitude the sun's centre sits at when it is said to rise or set.
///
/// Not zero. The disc has a radius of about 16 arcminutes and the atmosphere
/// refracts it upward by about 34 more, so the sun is *seen* on the horizon
/// while its centre is still half a degree below it. This is the standard
/// figure, and using 0.0 instead would put sunrise several minutes late all
/// year and considerably later at high latitudes.
pub const HORIZON_DEG: f64 = -0.833;

/// The sunrise and sunset that bracket the local day `jd` falls in, as Julian
/// dates.
///
/// **Julian dates, not hours, and always `sunrise < sunset`.** That is not
/// fussiness: San Francisco is eight hours west, so its sunset falls on the
/// *next* UTC day, and a scan of one UTC day returns a set from the previous
/// evening followed by a rise from this morning. Returning instants the caller
/// converts to whatever clock it keeps removes the question entirely.
///
/// `None` inside the polar circles when the sun neither rises nor sets — a real
/// answer, not a failure, and a caller who assumes otherwise will anchor a
/// routine to a time that does not exist.
///
/// Found by walking the day rather than by the closed-form hour-angle formula.
/// A minute-by-minute scan is a couple of thousand evaluations of arithmetic
/// that costs nanoseconds, needs no separate handling for the equation of time
/// or for the sun's declination moving during the day, and is obviously
/// correct — which the closed form is not.
pub fn sunrise_sunset(jd: f64, location: crate::Location) -> Option<(f64, f64)> {
    // Local solar midnight at or before `jd`, so the window covers one local
    // day rather than one Greenwich day.
    let midnight = jd - crate::time::local_solar_hours(jd, location.longitude_deg) / 24.0;

    let altitude_at = |minute: i32| {
        let at = midnight + f64::from(minute) / 1440.0;
        let lst = crate::time::local_sidereal_degrees(at, location.longitude_deg);
        crate::coords::equatorial_to_horizontal(position(at), lst, location.latitude_deg)
            .altitude_deg
    };

    let mut rise: Option<f64> = None;
    let mut previous = altitude_at(0);
    // Two days, so the set following a late rise is still inside the window.
    for minute in 1..=2880 {
        let current = altitude_at(minute);
        let at = midnight + f64::from(minute) / 1440.0;

        if rise.is_none() && previous < HORIZON_DEG && current >= HORIZON_DEG {
            rise = Some(at);
        } else if let Some(rise) = rise {
            if previous >= HORIZON_DEG && current < HORIZON_DEG {
                return Some((rise, at));
            }
        }
        previous = current;
    }

    None
}

/// How long the sun is up, in hours.
///
/// Zero in polar night and twenty-four in midnight sun — both of which are the
/// honest answer where [`sunrise_sunset`] has none to give.
pub fn daylight_hours(jd: f64, location: crate::Location) -> f64 {
    if let Some((rise, set)) = sunrise_sunset(jd, location) {
        return (set - rise) * 24.0;
    }

    let noon = jd.floor() + 1.0;
    let lst = crate::time::local_sidereal_degrees(noon, location.longitude_deg);
    let altitude =
        crate::coords::equatorial_to_horizontal(position(noon), lst, location.latitude_deg)
            .altitude_deg;
    if altitude > HORIZON_DEG {
        24.0
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coords::{culmination_altitude_deg, equatorial_to_horizontal};
    use crate::time::{julian_date, local_sidereal_degrees};
    use crate::SAN_FRANCISCO;

    /// The solstices and equinoxes are the four instants the sun's declination
    /// is known without any ephemeris at all: it is at the obliquity, zero,
    /// minus the obliquity, and zero again. Times are the published 2024
    /// instants in UTC.
    ///
    /// This pins the equation of centre, the obliquity and the ecliptic
    /// conversion together. Getting all four right by accident is not
    /// available.
    #[test]
    fn the_sun_reaches_the_declinations_the_seasons_are_defined_by() {
        let cases = [
            // (name, UTC instant, expected declination)
            ("March equinox", julian_date(2024, 3, 20, 3, 6, 0.0), 0.0),
            (
                "June solstice",
                julian_date(2024, 6, 20, 20, 51, 0.0),
                23.44,
            ),
            (
                "September equinox",
                julian_date(2024, 9, 22, 12, 44, 0.0),
                0.0,
            ),
            (
                "December solstice",
                julian_date(2024, 12, 21, 9, 21, 0.0),
                -23.44,
            ),
        ];

        for (name, jd, expected) in cases {
            let declination = position(jd).declination_deg;
            assert!(
                (declination - expected).abs() < 0.05,
                "{name}: declination {declination:.4}°, expected {expected}°"
            );
        }
    }

    /// A full circuit of the ecliptic in a year, and the direction of travel.
    /// A sign error here runs the seasons backwards while every single-instant
    /// test still passes.
    #[test]
    fn the_sun_advances_eastward_along_the_ecliptic() {
        let jd = julian_date(2024, 4, 1, 0, 0, 0.0);
        let today = ecliptic_longitude_deg(jd);
        let tomorrow = ecliptic_longitude_deg(jd + 1.0);
        let advance = (tomorrow - today).rem_euclid(360.0);
        assert!(
            (advance - 0.9856).abs() < 0.02,
            "the sun moved {advance:.4}° in a day"
        );
    }

    /// The number the whole feature is judged on, and it can be checked against
    /// a figure anyone can look up: the noon sun in San Francisco reaches about
    /// 75.7° in midsummer and about 28.8° at midwinter. Those are
    /// `90 - latitude ± obliquity`, so the test states the geometry and lets
    /// the ephemeris meet it.
    ///
    /// Solar noon is found by searching the day rather than assumed, which also
    /// exercises the sidereal chain that puts the sun on the meridian.
    #[test]
    fn the_noon_sun_over_san_francisco_reaches_its_published_solstice_altitudes() {
        let cases = [
            (julian_date(2024, 6, 20, 0, 0, 0.0), 23.44),
            (julian_date(2024, 12, 21, 0, 0, 0.0), -23.44),
        ];

        for (midnight, declination) in cases {
            // One-minute steps through the day; the sun's altitude has a single
            // maximum, so the largest sample is noon to within half a minute.
            let highest = (0..1440)
                .map(|minute| {
                    let jd = midnight + minute as f64 / 1440.0;
                    let lst = local_sidereal_degrees(jd, SAN_FRANCISCO.longitude_deg);
                    equatorial_to_horizontal(position(jd), lst, SAN_FRANCISCO.latitude_deg)
                        .altitude_deg
                })
                .fold(f64::MIN, f64::max);

            let expected = culmination_altitude_deg(declination, SAN_FRANCISCO.latitude_deg);
            assert!(
                (highest - expected).abs() < 0.2,
                "noon sun reached {highest:.3}°, geometry says {expected:.3}°"
            );
        }
    }

    /// Day length is the clean way to check sunrise and sunset, because it is
    /// the one figure that does not depend on the timezone, on daylight saving,
    /// or on the equation of time — all three cancel between the two ends.
    /// San Francisco gets 14h47m at the June solstice and 9h33m at the
    /// December one, both published.
    #[test]
    fn the_solstice_day_lengths_over_san_francisco_match_the_almanac() {
        let cases = [
            (julian_date(2024, 6, 20, 12, 0, 0.0), 14.0 + 47.0 / 60.0),
            (julian_date(2024, 12, 21, 12, 0, 0.0), 9.0 + 33.0 / 60.0),
        ];
        for (jd, published) in cases {
            let length = daylight_hours(jd, SAN_FRANCISCO);
            assert!(
                (length - published).abs() < 0.1,
                "day length {length:.3} h against a published {published:.3} h"
            );
        }
    }

    /// The sun rises in the morning and sets in the evening, and the gap moves
    /// with the season. A rise/set pair swapped would still give a plausible
    /// day length, so the order is worth stating separately.
    #[test]
    fn the_sun_rises_before_noon_and_sets_after_it_all_year() {
        for month in 1..=12 {
            let jd = julian_date(2024, month, 15, 12, 0, 0.0);
            let (rise, set) = sunrise_sunset(jd, SAN_FRANCISCO)
                .unwrap_or_else(|| panic!("month {month}: the sun should rise here"));
            assert!(set > rise, "month {month}: the sun set before it rose");

            // In *local solar* time, sunrise is always morning and sunset
            // always evening, whatever the season does to the exact hour.
            let rise_hour = crate::time::local_solar_hours(rise, SAN_FRANCISCO.longitude_deg);
            let set_hour = crate::time::local_solar_hours(set, SAN_FRANCISCO.longitude_deg);
            assert!(
                (4.5..8.0).contains(&rise_hour),
                "month {month}: sunrise at {rise_hour:.2} local"
            );
            assert!(
                (16.0..20.0).contains(&set_hour),
                "month {month}: sunset at {set_hour:.2} local"
            );
        }
    }

    /// Inside the polar circles the sun can fail to rise or fail to set, and
    /// that is an answer rather than an error. A routine anchored to sunrise
    /// would otherwise be placed at a time that does not exist.
    #[test]
    fn the_polar_summer_and_winter_are_answered_rather_than_failing() {
        let tromso = crate::Location {
            latitude_deg: 69.65,
            longitude_deg: 18.96,
        };
        let midsummer = julian_date(2024, 6, 21, 12, 0, 0.0);
        assert_eq!(sunrise_sunset(midsummer, tromso), None, "midnight sun");
        assert_eq!(daylight_hours(midsummer, tromso), 24.0);

        let midwinter = julian_date(2024, 12, 21, 12, 0, 0.0);
        assert_eq!(sunrise_sunset(midwinter, tromso), None, "polar night");
        assert_eq!(daylight_hours(midwinter, tromso), 0.0);
    }

    /// Midsummer days are long and midwinter days are short. Trivially true and
    /// worth stating, because it is the one assertion that fails if the whole
    /// chain is internally consistent but half a year out of phase.
    #[test]
    fn the_summer_day_is_longer_than_the_winter_day() {
        let daylight_minutes = |midnight: f64| {
            (0..1440)
                .filter(|minute| {
                    let jd = midnight + *minute as f64 / 1440.0;
                    let lst = local_sidereal_degrees(jd, SAN_FRANCISCO.longitude_deg);
                    equatorial_to_horizontal(position(jd), lst, SAN_FRANCISCO.latitude_deg)
                        .altitude_deg
                        > 0.0
                })
                .count()
        };

        let summer = daylight_minutes(julian_date(2024, 6, 20, 7, 0, 0.0));
        let winter = daylight_minutes(julian_date(2024, 12, 21, 8, 0, 0.0));
        assert!(
            summer > winter + 200,
            "summer {summer} minutes of daylight, winter {winter} — expected hours apart"
        );
        // San Francisco gets about 14h50m and 9h33m; well inside these bounds.
        assert!(
            (860..=910).contains(&summer),
            "summer daylight {summer} min"
        );
        assert!(
            (550..=600).contains(&winter),
            "winter daylight {winter} min"
        );
    }
}
