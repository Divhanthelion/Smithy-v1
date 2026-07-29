//! Turning where a thing *is* into where you would look to see it.

use crate::time::normalise_degrees;
use crate::{Equatorial, Horizontal};

/// Convert equatorial coordinates to the observer's horizon frame.
///
/// Azimuth is measured **from north, increasing eastward**, which is the
/// navigator's convention. Meeus measures from south, and mixing the two puts
/// everything 180° out — a sky that looks plausible and is exactly backwards.
///
/// Written as three components of a unit vector rather than as `atan` of a
/// ratio, because the ratio form loses the quadrant and needs a correction
/// whose condition is easy to get wrong. `atan2` cannot lose it.
pub fn equatorial_to_horizontal(
    object: Equatorial,
    local_sidereal_deg: f64,
    latitude_deg: f64,
) -> Horizontal {
    // The hour angle is how far the object is past the meridian.
    let hour_angle = (local_sidereal_deg - object.right_ascension_deg).to_radians();
    let dec = object.declination_deg.to_radians();
    let lat = latitude_deg.to_radians();

    let sin_altitude = dec.sin() * lat.sin() + dec.cos() * lat.cos() * hour_angle.cos();
    let altitude = sin_altitude.clamp(-1.0, 1.0).asin();

    let east = -dec.cos() * hour_angle.sin();
    let north = dec.sin() * lat.cos() - dec.cos() * lat.sin() * hour_angle.cos();

    Horizontal {
        altitude_deg: altitude.to_degrees(),
        azimuth_deg: normalise_degrees(east.atan2(north).to_degrees()),
    }
}

/// Convert ecliptic coordinates to equatorial ones.
///
/// The sun and moon are naturally described on the ecliptic; stars are
/// catalogued on the equator. This is the hinge between them.
pub fn ecliptic_to_equatorial(
    longitude_deg: f64,
    latitude_deg: f64,
    obliquity_deg: f64,
) -> Equatorial {
    let (lon, lat, obl) = (
        longitude_deg.to_radians(),
        latitude_deg.to_radians(),
        obliquity_deg.to_radians(),
    );

    let sin_dec = lat.sin() * obl.cos() + lat.cos() * obl.sin() * lon.sin();
    let y = lon.sin() * obl.cos() - lat.tan() * obl.sin();
    let x = lon.cos();

    Equatorial {
        right_ascension_deg: normalise_degrees(y.atan2(x).to_degrees()),
        declination_deg: sin_dec.clamp(-1.0, 1.0).asin().to_degrees(),
    }
}

/// The mean obliquity of the ecliptic, in degrees — the tilt that gives the
/// year its seasons. It shrinks by about half an arcsecond a year.
pub fn mean_obliquity_deg(days_since_j2000: f64) -> f64 {
    23.439_291 - 3.560e-7 * days_since_j2000
}

/// Angular separation between two points on the sky, in degrees.
pub fn separation_deg(a: Equatorial, b: Equatorial) -> f64 {
    let (ra1, dec1) = (
        a.right_ascension_deg.to_radians(),
        a.declination_deg.to_radians(),
    );
    let (ra2, dec2) = (
        b.right_ascension_deg.to_radians(),
        b.declination_deg.to_radians(),
    );
    let cos_sep = dec1.sin() * dec2.sin() + dec1.cos() * dec2.cos() * (ra1 - ra2).cos();
    cos_sep.clamp(-1.0, 1.0).acos().to_degrees()
}

/// The highest a body of this declination ever gets at this latitude.
///
/// Pure geometry, and the cleanest check there is on the sign conventions
/// above: it can be worked out on paper.
pub fn culmination_altitude_deg(declination_deg: f64, latitude_deg: f64) -> f64 {
    90.0 - (latitude_deg - declination_deg).abs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Location, SAN_FRANCISCO};

    fn at(ra: f64, dec: f64) -> Equatorial {
        Equatorial {
            right_ascension_deg: ra,
            declination_deg: dec,
        }
    }

    /// The single easiest way to check a sign convention: put something on the
    /// meridian and ask which way you would face. South of the zenith it is
    /// due south; north of the zenith, due north. Both, because getting one
    /// right by luck is common and getting both right by luck is not.
    #[test]
    fn an_object_on_the_meridian_is_due_south_below_the_zenith_and_due_north_above_it() {
        let lst = 100.0;
        let latitude = 37.7749;

        let south = equatorial_to_horizontal(at(lst, 0.0), lst, latitude);
        assert!(
            (south.azimuth_deg - 180.0).abs() < 1e-6,
            "azimuth {:.4}°, expected due south",
            south.azimuth_deg
        );

        let north = equatorial_to_horizontal(at(lst, 70.0), lst, latitude);
        assert!(
            north.azimuth_deg < 1e-6 || (north.azimuth_deg - 360.0).abs() < 1e-6,
            "azimuth {:.4}°, expected due north",
            north.azimuth_deg
        );
    }

    /// The other half of the convention. An object an hour east of the
    /// meridian has not got there yet, so it is still rising in the east; the
    /// sky turns westward. Reversing this is the mistake that produces a sky
    /// which runs backwards and otherwise looks entirely convincing.
    #[test]
    fn the_sky_turns_from_east_to_west() {
        let lst = 100.0;
        // Right ascension greater than the sidereal time means the object has
        // yet to reach the meridian — it is in the east.
        let rising = equatorial_to_horizontal(at(lst + 30.0, 0.0), lst, 37.7749);
        assert!(
            rising.azimuth_deg > 90.0 && rising.azimuth_deg < 180.0,
            "an object yet to culminate should be in the east, got {:.1}°",
            rising.azimuth_deg
        );

        let setting = equatorial_to_horizontal(at(lst - 30.0, 0.0), lst, 37.7749);
        assert!(
            setting.azimuth_deg > 180.0 && setting.azimuth_deg < 270.0,
            "an object past the meridian should be in the west, got {:.1}°",
            setting.azimuth_deg
        );
    }

    /// Culmination altitude is `90 - |latitude - declination|` by construction,
    /// so the transform has to reproduce it. This pins latitude handling
    /// against arithmetic that can be checked without a computer.
    #[test]
    fn a_body_culminates_at_the_altitude_plain_geometry_says_it_should() {
        let latitude = 37.7749;
        for declination in [-40.0, -23.44, 0.0, 23.44, 60.0, 89.0] {
            let lst = 123.456;
            let horizontal = equatorial_to_horizontal(at(lst, declination), lst, latitude);
            let expected = culmination_altitude_deg(declination, latitude);
            assert!(
                (horizontal.altitude_deg - expected).abs() < 1e-6,
                "declination {declination}° culminates at {:.4}°, geometry says {expected:.4}°",
                horizontal.altitude_deg
            );
        }
    }

    /// Polaris sits within a degree of the pole, so at San Francisco's latitude
    /// it never sets and stays near an altitude equal to that latitude. In
    /// Sydney it never rises. A latitude sign error passes every symmetric test
    /// and fails this one.
    #[test]
    fn the_pole_star_never_sets_in_the_north_and_never_rises_in_the_south() {
        let polaris = at(37.954_56, 89.264_11);
        let sydney = Location {
            latitude_deg: -33.8688,
            longitude_deg: 151.2093,
        };

        for lst in (0..360).step_by(15).map(f64::from) {
            let north = equatorial_to_horizontal(polaris, lst, SAN_FRANCISCO.latitude_deg);
            assert!(
                north.altitude_deg > 0.0,
                "Polaris below the horizon in San Francisco at LST {lst}°"
            );
            assert!(
                (north.altitude_deg - SAN_FRANCISCO.latitude_deg).abs() < 1.5,
                "Polaris at {:.2}°, expected near the latitude",
                north.altitude_deg
            );

            let south = equatorial_to_horizontal(polaris, lst, sydney.latitude_deg);
            assert!(
                south.altitude_deg < 0.0,
                "Polaris above the horizon in Sydney at LST {lst}°"
            );
        }
    }

    /// The ecliptic hinge, checked at the two places it is exactly known: the
    /// equinoxes sit on the equator, and the solstices at the obliquity.
    #[test]
    fn the_ecliptic_meets_the_equator_at_the_equinoxes() {
        let obliquity = 23.439_291;

        let spring = ecliptic_to_equatorial(0.0, 0.0, obliquity);
        assert!(spring.declination_deg.abs() < 1e-9);
        assert!(spring.right_ascension_deg.abs() < 1e-9);

        let summer = ecliptic_to_equatorial(90.0, 0.0, obliquity);
        assert!((summer.declination_deg - obliquity).abs() < 1e-6);

        let winter = ecliptic_to_equatorial(270.0, 0.0, obliquity);
        assert!((winter.declination_deg + obliquity).abs() < 1e-6);
    }

    #[test]
    fn separation_is_zero_for_a_point_against_itself_and_180_for_its_opposite() {
        let a = at(100.0, 20.0);
        assert!(separation_deg(a, a) < 1e-9);
        let opposite = at(280.0, -20.0);
        assert!((separation_deg(a, opposite) - 180.0).abs() < 1e-6);
    }
}
