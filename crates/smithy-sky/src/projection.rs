//! Flattening the visible hemisphere onto a disc.
//!
//! Stereographic, centred on the zenith — a planisphere. Azimuth becomes angle
//! and altitude becomes radius:
//!
//! ```text
//! r     = tan((90° − altitude) / 2)
//! theta = azimuth
//! ```
//!
//! Chosen over a gnomonic "window on the sky" because it shows the whole sky at
//! once, it is conformal so constellations keep their shapes, and it is what an
//! engraved planisphere actually is.
//!
//! **East is on the left.** A planisphere is held up and read against the sky,
//! so it is a view looking *up*, not a map looking down, and the east–west axis
//! is mirrored relative to a road map. Getting this backwards produces a sky
//! that is correct in every number and reflected as a picture — the sort of
//! error that survives a long time because nothing computes it.

use crate::Horizontal;

/// A point on the unit disc: the zenith at the origin, the horizon at radius 1.
///
/// Screen axes — `y` grows downward, as everything in the renderer does.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Projected {
    pub x: f64,
    pub y: f64,
}

impl Projected {
    /// Distance from the zenith, in `0.0..=1.0` for anything above the horizon.
    pub fn radius(&self) -> f64 {
        self.x.hypot(self.y)
    }
}

/// Project a horizon-frame direction onto the unit disc.
///
/// `None` below the horizon: the projection diverges there, and there is
/// nothing to draw under the ground anyway.
pub fn project(direction: Horizontal) -> Option<Projected> {
    if direction.altitude_deg <= 0.0 {
        return None;
    }
    let zenith_angle = (90.0 - direction.altitude_deg).to_radians();
    let radius = (zenith_angle / 2.0).tan();
    let azimuth = direction.azimuth_deg.to_radians();

    Some(Projected {
        // North up, east left — see the module note.
        x: -radius * azimuth.sin(),
        y: -radius * azimuth.cos(),
    })
}

/// The inverse, for testing that the forward map loses nothing.
pub fn unproject(point: Projected) -> Horizontal {
    let radius = point.radius();
    let zenith_angle = 2.0 * radius.atan();
    // `atan2` of the negated components undoes the axis flip above.
    let azimuth = (-point.x).atan2(-point.y).to_degrees().rem_euclid(360.0);

    Horizontal {
        altitude_deg: 90.0 - zenith_angle.to_degrees(),
        azimuth_deg: azimuth,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn look(altitude_deg: f64, azimuth_deg: f64) -> Horizontal {
        Horizontal {
            altitude_deg,
            azimuth_deg,
        }
    }

    /// The two fixed points of the projection. Overhead is the centre of the
    /// disc; the horizon is its rim. Everything else is interpolation between
    /// them, so if these are right the shape is right.
    #[test]
    fn the_zenith_is_the_centre_and_the_horizon_is_the_rim() {
        let zenith = project(look(90.0, 0.0)).expect("the zenith is above the horizon");
        assert!(
            zenith.radius() < 1e-9,
            "zenith at radius {}",
            zenith.radius()
        );

        // Approached from just above, since the horizon itself is excluded.
        let horizon = project(look(0.001, 0.0)).expect("just above the horizon");
        assert!(
            (horizon.radius() - 1.0).abs() < 1e-4,
            "horizon at radius {}",
            horizon.radius()
        );

        // Halfway up is *not* halfway out — that is the whole character of a
        // stereographic projection, and a linear one would read as wrong.
        let midway = project(look(45.0, 0.0)).expect("above the horizon");
        assert!(
            (midway.radius() - 0.414_213_56).abs() < 1e-6,
            "45° altitude at radius {}",
            midway.radius()
        );
    }

    /// The compass, stated as pixels. North at the top and **east at the left**
    /// — the mirroring that makes this a view of the sky rather than a map of
    /// the ground.
    #[test]
    fn north_is_up_and_east_is_to_the_left() {
        let north = project(look(45.0, 0.0)).unwrap();
        assert!(north.y < 0.0 && north.x.abs() < 1e-9, "north at {north:?}");

        let east = project(look(45.0, 90.0)).unwrap();
        assert!(east.x < 0.0 && east.y.abs() < 1e-9, "east at {east:?}");

        let south = project(look(45.0, 180.0)).unwrap();
        assert!(south.y > 0.0 && south.x.abs() < 1e-9, "south at {south:?}");

        let west = project(look(45.0, 270.0)).unwrap();
        assert!(west.x > 0.0 && west.y.abs() < 1e-9, "west at {west:?}");
    }

    /// Nothing below the horizon is drawn, and the boundary is excluded rather
    /// than clamped: the projection sends the horizon to radius 1 and anything
    /// under it to the far side of infinity.
    #[test]
    fn nothing_below_the_horizon_projects() {
        assert!(project(look(0.0, 0.0)).is_none());
        assert!(project(look(-0.001, 0.0)).is_none());
        assert!(project(look(-40.0, 123.0)).is_none());
    }

    /// A round trip must lose nothing, across the whole disc rather than at a
    /// convenient point.
    #[test]
    fn projecting_and_unprojecting_returns_the_same_direction() {
        for altitude in [0.5, 5.0, 23.0, 45.0, 67.5, 89.0] {
            for azimuth in (0..360).step_by(17).map(f64::from) {
                let original = look(altitude, azimuth);
                let round_trip = unproject(project(original).expect("above the horizon"));
                assert!(
                    (round_trip.altitude_deg - altitude).abs() < 1e-9,
                    "altitude {altitude} came back as {}",
                    round_trip.altitude_deg
                );
                let drift = (round_trip.azimuth_deg - azimuth)
                    .abs()
                    .min(360.0 - (round_trip.azimuth_deg - azimuth).abs());
                assert!(
                    drift < 1e-9,
                    "azimuth {azimuth} came back as {}",
                    round_trip.azimuth_deg
                );
            }
        }
    }
}
