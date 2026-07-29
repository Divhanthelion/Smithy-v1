//! Where everything in the sky is, for a place and an instant.
//!
//! This crate knows nothing about drawing, and has no dependencies at all — the
//! same separation `smithy-agent` has from the UI, and for the same reason.
//! [`SkyState::at`] answers "where is everything", the renderer decides what
//! that should look like, and the two can be wrong independently.
//!
//! That separation is the point of choosing this feature. Almost every visual
//! decision in this project is unverifiable without a screenshot; astronomy has
//! a **correct answer**. Whether the noon sun reaches 75.7° over San Francisco
//! at midsummer is a fact, and so is whether Sirius is up at ten o'clock in
//! February. So the machinery underneath is tested against published values and
//! only the *look* needs eyes on it.
//!
//! ```no_run
//! use smithy_sky::{SkyState, SAN_FRANCISCO};
//! let sky = SkyState::now(SAN_FRANCISCO);
//! println!("the sun is {:.1}° up", sky.sun.horizontal.altitude_deg);
//! ```

pub mod catalogue;
pub mod coords;
pub mod moon;
pub mod projection;
pub mod sun;
pub mod time;

use projection::Projected;

/// Somewhere on the earth. East longitude and north latitude are positive.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Location {
    pub latitude_deg: f64,
    pub longitude_deg: f64,
}

/// The default, per the plan.
pub const SAN_FRANCISCO: Location = Location {
    latitude_deg: 37.7749,
    longitude_deg: -122.4194,
};

/// A direction in the sky, fixed to the stars.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Equatorial {
    pub right_ascension_deg: f64,
    pub declination_deg: f64,
}

/// A direction relative to the observer's horizon. Azimuth runs from north,
/// increasing eastward.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Horizontal {
    pub altitude_deg: f64,
    pub azimuth_deg: f64,
}

/// How light it is, by the sun's altitude rather than by the clock.
///
/// The boundaries are the standard definitions of twilight, not invented
/// numbers — which is exactly why they can be tested.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Sun above +6°.
    Day,
    /// Sun between +6° and 0°: low, warm, long shadows.
    GoldenHour,
    /// Sun 0° to −6°. The brightest stars appear.
    CivilTwilight,
    /// Sun −6° to −12°. Most stars; the horizon is still visible at sea.
    NauticalTwilight,
    /// Sun −12° to −18°. The full field, still graded.
    AstronomicalTwilight,
    /// Sun below −18°. As dark as it gets.
    Night,
}

impl Phase {
    /// The phase for a sun altitude, in degrees.
    pub fn for_sun_altitude(altitude_deg: f64) -> Self {
        match altitude_deg {
            a if a > 6.0 => Phase::Day,
            a if a > 0.0 => Phase::GoldenHour,
            a if a > -6.0 => Phase::CivilTwilight,
            a if a > -12.0 => Phase::NauticalTwilight,
            a if a > -18.0 => Phase::AstronomicalTwilight,
            _ => Phase::Night,
        }
    }

    /// How dark it is, from 0 in full day to 1 in full night.
    ///
    /// Continuous, because the sky's colour has to slide rather than jump:
    /// stepping between six discrete colours at the boundaries would be visible
    /// as the backdrop flicking to a new shade while you watched.
    pub fn darkness(sun_altitude_deg: f64) -> f64 {
        // Mapped across the whole twilight range, from the top of golden hour
        // to the bottom of astronomical twilight.
        ((6.0 - sun_altitude_deg) / 24.0).clamp(0.0, 1.0)
    }
}

/// A body, where it is and where to draw it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Body {
    pub horizontal: Horizontal,
    /// `None` when below the horizon, which is also when not to draw it.
    pub projected: Option<Projected>,
}

/// A star that is up, already projected.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VisibleStar {
    /// Harvard Revised number, so a star can be identified without relying on
    /// a common name the catalogue does not carry.
    pub hr: u16,
    pub name: &'static str,
    pub position: Projected,
    pub magnitude: f32,
    pub colour_index: f32,
}

/// Everything the renderer needs, and no opinions about how it should look.
#[derive(Debug, Clone, PartialEq)]
pub struct SkyState {
    pub sun: Body,
    pub moon: Body,
    /// 0 at new, 1 at full.
    pub moon_illumination: f64,
    /// Which limb is lit. A backwards crescent is the one lunar error every
    /// reader spots.
    pub moon_waxing: bool,
    pub phase: Phase,
    /// 0 in full day, 1 in full night.
    pub darkness: f64,
    /// Every catalogued star above the horizon, projected onto the unit disc.
    pub stars: Vec<VisibleStar>,
    /// Local mean solar time, hours since local midnight. Shared with the
    /// fisherman, who keeps the same clock.
    pub solar_hours: f64,
}

impl SkyState {
    /// The sky over `location` at a Julian date.
    pub fn at(location: Location, jd: f64) -> Self {
        let lst = time::local_sidereal_degrees(jd, location.longitude_deg);
        let latitude = location.latitude_deg;

        let body = |equatorial| {
            let horizontal = coords::equatorial_to_horizontal(equatorial, lst, latitude);
            Body {
                horizontal,
                projected: projection::project(horizontal),
            }
        };

        let sun_body = body(sun::position(jd));
        let stars = catalogue::STARS
            .iter()
            .filter_map(|star| {
                let horizontal = coords::equatorial_to_horizontal(
                    Equatorial {
                        right_ascension_deg: star.right_ascension_deg,
                        declination_deg: star.declination_deg,
                    },
                    lst,
                    latitude,
                );
                projection::project(horizontal).map(|position| VisibleStar {
                    hr: star.hr,
                    name: star.name,
                    position,
                    magnitude: star.magnitude,
                    colour_index: star.colour_index,
                })
            })
            .collect();

        Self {
            phase: Phase::for_sun_altitude(sun_body.horizontal.altitude_deg),
            darkness: Phase::darkness(sun_body.horizontal.altitude_deg),
            sun: sun_body,
            moon: body(moon::position(jd)),
            moon_illumination: moon::illuminated_fraction(jd),
            moon_waxing: moon::is_waxing(jd),
            stars,
            solar_hours: time::local_solar_hours(jd, location.longitude_deg),
        }
    }

    /// The sky right now.
    ///
    /// The only place this crate reads a clock. Everything else takes a Julian
    /// date, which is what makes the whole thing testable.
    pub fn now(location: Location) -> Self {
        let seconds = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            // A clock before 1970 is a broken clock, not a reason to panic in a
            // backdrop. Fall back to the epoch and draw *a* sky.
            .unwrap_or(0.0);
        Self::at(location, time::julian_date_from_unix(seconds))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::julian_date;

    /// Harvard Revised number for Sirius.
    const SIRIUS: u16 = 2491;

    /// The boundaries are the published definitions of twilight, and the whole
    /// sky's colour hangs off them. Checked on both sides of each, because a
    /// comparison written the wrong way round still puts the boundary in the
    /// right place.
    #[test]
    fn the_phase_boundaries_are_the_standard_twilight_definitions() {
        assert_eq!(Phase::for_sun_altitude(45.0), Phase::Day);
        assert_eq!(Phase::for_sun_altitude(6.1), Phase::Day);
        assert_eq!(Phase::for_sun_altitude(5.9), Phase::GoldenHour);
        assert_eq!(Phase::for_sun_altitude(0.1), Phase::GoldenHour);
        assert_eq!(Phase::for_sun_altitude(-0.1), Phase::CivilTwilight);
        assert_eq!(Phase::for_sun_altitude(-5.9), Phase::CivilTwilight);
        assert_eq!(Phase::for_sun_altitude(-6.1), Phase::NauticalTwilight);
        assert_eq!(Phase::for_sun_altitude(-11.9), Phase::NauticalTwilight);
        assert_eq!(Phase::for_sun_altitude(-12.1), Phase::AstronomicalTwilight);
        assert_eq!(Phase::for_sun_altitude(-17.9), Phase::AstronomicalTwilight);
        assert_eq!(Phase::for_sun_altitude(-18.1), Phase::Night);
        assert_eq!(Phase::for_sun_altitude(-90.0), Phase::Night);
    }

    /// Darkness has to slide, not step. Six discrete colours would be visible
    /// as the backdrop flicking to a new shade while you watched it.
    #[test]
    fn darkness_runs_smoothly_from_day_to_night() {
        assert_eq!(Phase::darkness(30.0), 0.0);
        assert_eq!(Phase::darkness(6.0), 0.0);
        assert_eq!(Phase::darkness(-18.0), 1.0);
        assert_eq!(Phase::darkness(-60.0), 1.0);
        assert!((Phase::darkness(-6.0) - 0.5).abs() < 1e-9);

        let mut previous = 0.0;
        for tenths in (-200..=100).rev() {
            let darkness = Phase::darkness(f64::from(tenths) / 10.0);
            assert!(darkness >= previous, "darkness went backwards at {tenths}");
            previous = darkness;
        }
    }

    /// The end-to-end check the plan asked for, and one a person can confirm by
    /// walking outside. Sirius is the winter evening star of the northern
    /// hemisphere: high at ten o'clock in February, and below the horizon at
    /// the same hour in July.
    ///
    /// It runs the whole chain — Julian date, sidereal time, the horizon
    /// transform and the catalogue — against a fact rather than against itself.
    ///
    /// **Same clock time, six months apart**, which is the version that
    /// discriminates. An earlier draft compared a February evening against a
    /// July *afternoon* and expected Sirius to be gone; it is not gone, it is
    /// merely invisible, because it sits about thirteen degrees from the sun in
    /// mid-July and is above the horizon in broad daylight. `stars` reports
    /// what is above the horizon, not what the eye could pick out — deciding
    /// that is the renderer's job, and it fades them by `darkness`.
    #[test]
    fn sirius_is_high_on_a_february_evening_and_below_the_horizon_in_july() {
        // 06:00 UTC is about ten in the evening, solar, at this longitude —
        // and solar time depends only on where you are, so the *same* UTC hour
        // is the same hour of the evening in both seasons. That is what makes
        // this a comparison of the sky rather than of the clock.
        let february = SkyState::at(SAN_FRANCISCO, julian_date(2024, 2, 3, 6, 0, 0.0));
        let sirius = february
            .stars
            .iter()
            .find(|s| s.hr == SIRIUS)
            .expect("Sirius is up on a February evening");
        assert!(
            sirius.position.radius() < 1.0,
            "a visible star must project inside the disc"
        );
        assert_eq!(february.phase, Phase::Night, "ten o'clock in February");

        let july = SkyState::at(SAN_FRANCISCO, julian_date(2024, 7, 16, 6, 0, 0.0));
        assert!(
            !july.stars.iter().any(|s| s.hr == SIRIUS),
            "Sirius has set by ten o'clock in July"
        );
        assert_eq!(july.phase, Phase::Night, "ten o'clock in July");
    }

    /// Half the sky is up at any moment, so a plausible fraction of the
    /// catalogue should be. Nothing visible means a transform collapsed;
    /// everything visible means the horizon test is not being applied.
    #[test]
    fn about_half_the_catalogue_is_above_the_horizon_at_any_time() {
        let total = catalogue::STARS.len();
        for hour in (0..24).step_by(3) {
            let sky = SkyState::at(SAN_FRANCISCO, julian_date(2024, 5, 5, hour, 0, 0.0));
            let visible = sky.stars.len();
            assert!(
                visible > total / 5 && visible < total * 4 / 5,
                "{visible} of {total} stars up at {hour}:00 UTC"
            );
        }
    }

    /// Every star handed to the renderer must be inside the disc it draws on.
    /// One escaping would be painted outside the instrument, which is the sort
    /// of thing that looks like a rendering bug for a long time.
    #[test]
    fn every_visible_star_projects_inside_the_disc() {
        for day in [1.0, 90.0, 180.0, 270.0] {
            let sky = SkyState::at(SAN_FRANCISCO, time::J2000 + day);
            for star in &sky.stars {
                assert!(
                    star.position.radius() <= 1.0,
                    "{} projected to radius {:.4}",
                    star.name,
                    star.position.radius()
                );
            }
        }
    }

    /// The moon is up about as often as it is down, and its phase cycles. A
    /// state that never produced a visible moon would leave the disc missing
    /// with nothing to show why.
    #[test]
    fn the_moon_is_sometimes_up_and_its_phase_varies() {
        let mut seen_up = false;
        let mut brightest: f64 = 0.0;
        let mut darkest: f64 = 1.0;

        for day in 0..30 {
            let sky = SkyState::at(
                SAN_FRANCISCO,
                julian_date(2024, 6, 1, 4, 0, 0.0) + f64::from(day),
            );
            seen_up |= sky.moon.projected.is_some();
            brightest = brightest.max(sky.moon_illumination);
            darkest = darkest.min(sky.moon_illumination);
        }

        assert!(seen_up, "the moon was never above the horizon in a month");
        assert!(brightest > 0.9, "the moon never got near full");
        assert!(darkest < 0.1, "the moon never got near new");
    }
}
