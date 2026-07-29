//! Where the moon is, and how much of it is lit.
//!
//! The moon is the hardest body in the sky to compute well — the full theory
//! runs to hundreds of periodic terms. This is the low-precision version:
//! the leading term in longitude and latitude only, good to roughly a degree.
//!
//! That is honest for what it draws. A degree is twice the moon's own width, so
//! **the disc is in the right part of the sky and not the right place in it**.
//! The phase is far better than the position, because phase depends on the
//! angle between the sun and moon — around 180° at full and 0° at new, where
//! the illuminated fraction changes so slowly with angle that a degree of error
//! moves it by well under a percent.

use crate::coords::{ecliptic_to_equatorial, mean_obliquity_deg};
use crate::sun;
use crate::time::{days_since_j2000, normalise_degrees};
use crate::Equatorial;

/// The moon's approximate equatorial position at a Julian date.
pub fn position(jd: f64) -> Equatorial {
    let n = days_since_j2000(jd);
    let (longitude, latitude) = ecliptic_position_deg(n);
    ecliptic_to_equatorial(longitude, latitude, mean_obliquity_deg(n))
}

/// Ecliptic longitude and latitude, in degrees.
fn ecliptic_position_deg(days_since_j2000: f64) -> (f64, f64) {
    let n = days_since_j2000;
    // Mean longitude, mean anomaly, and argument of latitude — the moon's
    // position round its orbit, how far it is from perigee, and how far from
    // the node where its tilted orbit crosses the ecliptic.
    let mean_longitude = normalise_degrees(218.316 + 13.176_396 * n);
    let mean_anomaly = normalise_degrees(134.963 + 13.064_993 * n).to_radians();
    let argument_of_latitude = normalise_degrees(93.272 + 13.229_350 * n).to_radians();

    (
        normalise_degrees(mean_longitude + 6.289 * mean_anomaly.sin()),
        5.128 * argument_of_latitude.sin(),
    )
}

/// The fraction of the moon's disc that is lit, from 0 at new to 1 at full.
///
/// Derived from the elongation — the angle between the sun and the moon as seen
/// from here. That is why this is far more accurate than the position it is
/// computed from: near new and full the illuminated fraction is stationary with
/// respect to the angle, so the error is squashed rather than carried.
pub fn illuminated_fraction(jd: f64) -> f64 {
    let elongation = elongation_deg(jd).to_radians();
    ((1.0 - elongation.cos()) / 2.0).clamp(0.0, 1.0)
}

/// Whether the moon is waxing — lit on the side it will grow into.
///
/// Illumination alone cannot say: a half moon is half lit going up and half lit
/// coming down, and drawing the terminator on the wrong side is the one lunar
/// mistake everybody notices.
pub fn is_waxing(jd: f64) -> bool {
    let n = days_since_j2000(jd);
    let (moon_longitude, _) = ecliptic_position_deg(n);
    // Waxing while the moon is running ahead of the sun by less than half a
    // circle: from new, through first quarter, to full.
    normalise_degrees(moon_longitude - sun::ecliptic_longitude_deg(jd)) < 180.0
}

/// The angle between the sun and the moon, in degrees: 0 at new, 180 at full.
pub fn elongation_deg(jd: f64) -> f64 {
    let n = days_since_j2000(jd);
    let (moon_longitude, moon_latitude) = ecliptic_position_deg(n);
    let obliquity = mean_obliquity_deg(n);

    crate::coords::separation_deg(
        ecliptic_to_equatorial(moon_longitude, moon_latitude, obliquity),
        ecliptic_to_equatorial(sun::ecliptic_longitude_deg(jd), 0.0, obliquity),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::julian_date;

    /// Published new and full moons for 2024, in UTC. New moon means the sun
    /// and moon share a longitude and nothing is lit; full means they are
    /// opposite and all of it is.
    ///
    /// The tolerances are loose on purpose and still discriminating: with a
    /// degree of error in longitude, a genuine new moon computes to about
    /// 0.0001 illuminated and a genuine full moon to about 0.9999, so anything
    /// outside these bounds is a real fault rather than the known imprecision.
    #[test]
    fn the_moon_is_dark_at_new_and_lit_at_full() {
        let new_moons = [
            julian_date(2024, 1, 11, 11, 57, 0.0),
            julian_date(2024, 5, 8, 3, 22, 0.0),
            julian_date(2024, 9, 3, 1, 55, 0.0),
        ];
        for jd in new_moons {
            let lit = illuminated_fraction(jd);
            assert!(lit < 0.03, "new moon computed {lit:.4} illuminated");
        }

        let full_moons = [
            julian_date(2024, 1, 25, 17, 54, 0.0),
            julian_date(2024, 5, 23, 13, 53, 0.0),
            julian_date(2024, 9, 18, 2, 34, 0.0),
        ];
        for jd in full_moons {
            let lit = illuminated_fraction(jd);
            assert!(lit > 0.97, "full moon computed {lit:.4} illuminated");
        }
    }

    /// A synodic month is 29.53 days, so the moon must be back where it started
    /// after one. This catches a rate error that no single instant would.
    ///
    /// Only three cycles, and that is a statement about the *test* rather than
    /// about the model. 29.530589 days is a **mean**: the real interval between
    /// new moons swings by up to seven hours either side of it, because the
    /// moon's orbit is eccentric. Multiply the mean out far enough and it stops
    /// predicting the actual moon — measured, the elongation comes back within
    /// about a degree for two cycles and then walks off at three or four
    /// degrees a cycle. Extending the loop would be testing arithmetic on a
    /// mean, not this crate.
    ///
    /// The model's real accuracy is pinned by
    /// `the_moon_is_dark_at_new_and_lit_at_full`, which uses published new and
    /// full moons eight months apart and does not rely on any mean at all.
    #[test]
    fn the_phase_returns_after_one_synodic_month() {
        let jd = julian_date(2024, 3, 1, 0, 0, 0.0);
        let synodic = 29.530_589;
        for cycles in 1..=3 {
            let later = jd + synodic * f64::from(cycles);
            let drift = (illuminated_fraction(later) - illuminated_fraction(jd)).abs();
            assert!(
                drift < 0.02,
                "after {cycles} months the phase drifted by {drift:.4}"
            );
        }
    }

    /// Between new and full the moon waxes, and between full and new it wanes.
    /// The terminator is drawn on whichever side this says, and a reader would
    /// spot a backwards crescent immediately.
    #[test]
    fn the_moon_waxes_from_new_to_full_and_wanes_back() {
        let new_moon = julian_date(2024, 1, 11, 11, 57, 0.0);
        assert!(is_waxing(new_moon + 3.0), "should wax three days after new");
        assert!(is_waxing(new_moon + 10.0), "should wax approaching full");
        assert!(!is_waxing(new_moon + 18.0), "should wane after full");
        assert!(!is_waxing(new_moon + 26.0), "should wane approaching new");
    }

    /// Illumination rises and falls with elongation, monotonically, which is
    /// the property the drawing depends on when it interpolates a terminator.
    #[test]
    fn illumination_grows_with_elongation() {
        let new_moon = julian_date(2024, 1, 11, 11, 57, 0.0);
        let mut previous = illuminated_fraction(new_moon);
        for day in 1..=14 {
            let lit = illuminated_fraction(new_moon + f64::from(day));
            assert!(
                lit > previous,
                "day {day}: {lit:.4} did not exceed the previous {previous:.4}"
            );
            previous = lit;
        }
    }
}
