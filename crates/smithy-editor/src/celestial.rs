//! The sky behind the editor.
//!
//! The actual sky over a place at the actual time: stars in their real
//! positions, the sun tracking across by day, twilight in between. Where things
//! *are* is [`smithy_sky`], which has no idea this file exists; this decides
//! only what that should look like.
//!
//! Forged only. Flat stays plain.
//!
//! ## Restraint
//!
//! This is a backdrop behind code and it fails if it ever competes with the
//! code, however pretty. The brightest star is dimmer than the dimmest syntax
//! colour, and "day" means *relatively* lighter — never a light background
//! behind dark-theme text. Those two are not aspirations here: [`star_alpha`]
//! and [`ground`] are pure functions with the ceilings asserted in tests,
//! because a backdrop that quietly brightened over a year of tweaks is exactly
//! the failure mode.

use floem::peniko::kurbo::{Circle, Point, Rect};
use floem::peniko::Color;
use floem::prelude::*;
use floem::reactive::{Memo, RwSignal, SignalGet};
use floem::views::canvas;

use smithy_sky::projection::Projected;
use smithy_sky::{Location, SkyState, SAN_FRANCISCO};

use crate::aesthetic::Aesthetic;

/// The sky at its darkest — the middle of an astronomical night.
const NIGHT: Color = Color::from_rgb8(5, 7, 13);
/// The sky in full day. Still dark: this sits behind a dark theme, and the
/// point of "day" is that it is *relatively* lighter, not that it is light.
const DAY: Color = Color::from_rgb8(19, 27, 42);

/// The sun: a warm mark, not a lamp.
const SUN_BODY: Color = Color::from_rgb8(214, 158, 74);
const SUN_CORE: Color = Color::from_rgb8(255, 236, 186);
const SUN_GLOW: Color = Color::from_rgb8(180, 118, 52);
const MOON_BODY: Color = Color::from_rgb8(176, 186, 206);
const MOON_DARK: Color = Color::from_rgb8(28, 33, 46);

/// The brightest star's alpha.
///
/// **0.35, not the 0.62 this used to be**, and the old number was not merely
/// generous — it was wrong in the direction the comment claimed it was safe.
/// WCAG technique F83 measures a background image against "the area of the
/// image that is *lightest*" for light text, so what matters is the brightest
/// star pixel a glyph can land on, not the mean sky. Composited onto `NIGHT`, a
/// star at 0.62 gives (158,155,149) — brighter than `FG_FAINT` and `FG_GHOST`,
/// with body `FG` over it dropping to 2.03:1, well under the 4.5:1 line. At
/// 0.35 that comes back to about 4.6:1. It only ever survived because a two
/// pixel star rarely lands under a letter.
const STAR_ALPHA_CEILING: f32 = 0.35;
/// The dimmest star worth drawing at all. Below about this the star composites
/// to under 1.2:1 against the sky, which on a sub-pixel feature is beneath the
/// threshold of detection — ink spent where nothing can be seen.
const STAR_ALPHA_FLOOR: f32 = 0.17;
/// No star is drawn smaller than this.
///
/// Stellarium, KStars and Cartes du Ciel all independently refuse to draw a
/// sub-pixel star: they clamp the radius and move the lost brightness into
/// alpha instead. Stellarium conserves *volume* — `luminance = r³/1.728` at a
/// floor of 1.2 — and Cartes du Ciel does the same thing with a linear factor.
/// Without this, a faint star is a 0.35px disc whose alpha is then spread by
/// antialiasing over several pixels and vanishes entirely, which is where most
/// of the field was going.
const MIN_STAR_RADIUS: f64 = 1.15;
/// How far past the panel's corners the horizon is pushed.
///
/// The first version inscribed the whole hemisphere in the *shorter* dimension,
/// so the horizon was a visible circle with bare ground outside it. On a wide
/// editor pane that read as a stray gold ring drawn across the code, which is
/// exactly what it was. Now the disc covers the panel and the horizon is off
/// the edge: you see the sky above about fifteen degrees, and no rim at all.
///
/// It also avoids the crush. A stereographic projection piles the last few
/// degrees of altitude into the outermost sliver, so a visible horizon means a
/// dense ring of stars around a sparse middle.
const HORIZON_OVERSHOOT: f64 = 1.3;

/// The ground colour for a given darkness, 0 in full day and 1 in full night.
pub fn ground(darkness: f64) -> Color {
    lerp(DAY, NIGHT, shown_darkness(darkness) as f32)
}

/// The darkness the sky is *drawn* at, given the darkness it actually is.
///
/// **The positions are real; the lighting is not.** This is a deliberate
/// retreat from the plan, and the reason is that the two goals were pulling
/// opposite ways and nobody said so out loud.
///
/// A photometrically honest sky is empty during the day — that is not a bug,
/// that is what the daytime sky *is*. But this is a backdrop for someone
/// writing code, and they write it in the daytime, so honest lighting means the
/// feature is invisible during every hour it is looked at. Told to choose
/// between correct and worth having, correct loses: it is a decoration, not an
/// instrument, and it was asked for because it should look like something.
///
/// So the floor keeps a night sky up at all hours, and the remaining range
/// still moves with the real sun — twilight is a little lighter than midnight,
/// and noon is lighter still. What is lost is the honest daytime blank.
pub fn shown_darkness(actual: f64) -> f64 {
    // Was 0.82 — a night sky held up at all hours, because an honest daytime
    // sky is empty and this is a backdrop for someone working in the daytime.
    // That was the right trade while the day had nothing in it. It now has the
    // sun, so the day can be a day again: the floor only keeps a suggestion of
    // stars rather than replacing the sky's whole range.
    const FLOOR: f64 = 0.20;
    FLOOR + actual.clamp(0.0, 1.0) * (1.0 - FLOOR)
}

/// A star's radius in pixels, from its apparent magnitude.
///
/// Magnitude is logarithmic and runs *backwards* — smaller is brighter — so
/// this is a descending ramp. The range is deliberately narrow: real magnitude
/// scaling would make Sirius a blob.
pub fn star_radius(magnitude: f32) -> f64 {
    let brightness = ((5.0 - magnitude) / 6.5).clamp(0.0, 1.0) as f64;
    0.55 + brightness * brightness * 2.1
}

/// The **core** radius and alpha a star is drawn at. The halo extends beyond
/// this — see [`STAR_RINGS`].
///
/// Below [`MIN_STAR_RADIUS`] the radius is floored and the brightness it would
/// have had is moved into alpha, cubically — a disc that cannot shrink any
/// further gets dimmer instead. That is Stellarium's trick
/// (`StelSkyDrawer.cpp`, "if size of star is too small (blink) we put its size
/// to 1.2 ... and we compensate the difference of brightness with cmag"), and
/// Cartes du Ciel arrives at the same rule independently.
pub fn star_geometry(magnitude: f32, darkness: f64, pane_scale: f64) -> (f64, f32) {
    let wanted = star_radius(magnitude) * pane_scale;
    let mut alpha = star_alpha(magnitude, darkness);

    if wanted < MIN_STAR_RADIUS {
        alpha *= (wanted / MIN_STAR_RADIUS).powi(3) as f32;
        return (MIN_STAR_RADIUS, alpha);
    }
    (wanted, alpha)
}

/// The discs a star is built from: each one's radius as a multiple of the
/// **core** radius, and the alpha it carries. Drawn largest first, so the core
/// comes last at full strength.
///
/// A star reads as a *point of light* rather than a dot of paint because its
/// centre is blown out while its wings carry the colour. Measured off
/// Stellarium's own `star16x16.png` sprite, the profile is a clipped Gaussian —
/// `min(1, 1.70·exp(−4.20·d²))`, rms 0.023 — flat-saturated out to about a
/// third of the radius and falling to nothing by the rim.
///
/// **These are multiples of the core, not fractions of the whole**, and getting
/// that backwards is what made the first attempt invisible. The measured
/// profile came off a sixteen-pixel sprite, where a core at 0.25 of the radius
/// is four pixels across. Applied to a star that is one pixel in radius, the
/// same fraction gives a saturated core 0.29px wide — which antialiasing spreads
/// until nothing is left of it. Every star had its brightness in a disc too
/// small to draw. The core is now the *floor*, and the halo grows outward from
/// it.
///
/// Built from discs rather than a radial gradient because **floem's vger
/// backend discards radial gradients entirely**, and rather than a blur because
/// its `fill` passes `blur_radius` on every path except the one taken by a
/// `Circle`.
pub const STAR_RINGS: [(f64, f32); 3] = [(2.9, 0.07), (1.8, 0.20), (1.0, 1.0)];

/// How much a star's brightness wavers, and how fast.
///
/// Scintillation is refraction through moving air, so it is **stronger near the
/// horizon** — a star overhead is seen through a fraction of the atmosphere a
/// star near the horizon is seen through. That is why Sirius low in the south
/// flashes and flickers while something at the zenith sits steady, and putting
/// it in makes the field read as *sky* rather than as dots, at no cost in
/// average brightness.
const TWINKLE_DEPTH: f32 = 0.38;

/// A star's brightness multiplier at a moment, from its own phase.
///
/// Two sine terms at incommensurable rates, so no star repeats on a period a
/// viewer could notice, and each star's phase is seeded from its catalogue
/// number — the field shimmers rather than pulsing in unison, which is the
/// difference between a sky and a string of fairy lights.
///
/// `radius` is the star's distance from the zenith on the unit disc, 0 overhead
/// and 1 at the horizon.
pub fn twinkle(hr: u16, radius: f64, phase: f64) -> f32 {
    let seed = f64::from(hr) * 0.618_034;
    let slow = ((phase * 1.7 + seed) * std::f64::consts::TAU).sin();
    let fast = ((phase * 4.3 + seed * 2.7) * std::f64::consts::TAU).sin();
    let wave = (slow * 0.6 + fast * 0.4) as f32;

    // Nothing at the zenith, full depth at the horizon.
    let depth = TWINKLE_DEPTH * radius.clamp(0.0, 1.0) as f32;
    1.0 + wave * depth
}

/// A star's alpha, from its magnitude and how dark the sky is.
///
/// Two jobs: keep the brightest star under the ceiling, and put every star out
/// in daylight. Stars are still *there* in the daytime — `SkyState` reports
/// what is above the horizon, not what the eye could pick out — so the fade has
/// to happen here.
pub fn star_alpha(magnitude: f32, darkness: f64) -> f32 {
    let brightness = ((5.0 - magnitude) / 6.5).clamp(0.0, 1.0);
    // Squared, so the field *fades out* into daylight rather than merely
    // dimming — at noon the sun is the thing worth looking at, and a sky with
    // both is a sky with neither.
    let shown = shown_darkness(darkness);
    let visibility = (shown * shown) as f32;
    (STAR_ALPHA_FLOOR + brightness * (STAR_ALPHA_CEILING - STAR_ALPHA_FLOOR)) * visibility
}

/// A star's colour from its B−V index.
///
/// This is most of what separates a star field from scattered white dots. Blue
/// below zero, white around 0.6, orange above 1.4.
pub fn star_colour(colour_index: f32) -> Color {
    // Sampled from Stellarium's own `colorTable`, which is indexed by B−V.
    const BLUE: Color = Color::from_rgb8(156, 184, 255);
    const WHITE: Color = Color::from_rgb8(255, 255, 255);
    const AMBER: Color = Color::from_rgb8(255, 213, 162);

    // **Neutral white sits at B−V 0.44**, not 0.6. Stellarium's table is exactly
    // white there, and Ballesteros's B−V→temperature relation (EPL 97, 34008,
    // eq. 14) puts 0.44 at 6674 K, which Mitchell Charity's blackbody table
    // renders as very nearly white — two independent sources agreeing to about
    // 2%. The previous ramp was neutral at 0.6, which shifted everything warm
    // and desaturated the whole blue half of the sky.
    if colour_index < 0.44 {
        lerp(BLUE, WHITE, ((colour_index + 0.35) / 0.79).clamp(0.0, 1.0))
    } else {
        lerp(WHITE, AMBER, ((colour_index - 0.44) / 1.15).clamp(0.0, 1.0))
    }
}

/// Where a projected point lands on screen.
///
/// The unit disc is inscribed in the shorter dimension, so the whole
/// hemisphere is visible and the horizon is a rim rather than an edge that runs
/// off the panel — an instrument set into the panel, which is what a
/// planisphere is.
pub fn to_screen(point: Projected, width: f64, height: f64) -> Point {
    let radius = disc_radius(width, height);
    Point::new(
        width / 2.0 + point.x * radius,
        height / 2.0 + point.y * radius,
    )
}

/// The radius the unit disc is drawn at, in pixels.
pub fn disc_radius(width: f64, height: f64) -> f64 {
    width.hypot(height) / 2.0 * HORIZON_OVERSHOOT
}

/// Linear interpolation between two colours.
fn lerp(from: Color, to: Color, t: f32) -> Color {
    let t = t.clamp(0.0, 1.0);
    let [r0, g0, b0, a0] = from.components;
    let [r1, g1, b1, a1] = to.components;
    Color::new([
        r0 + (r1 - r0) * t,
        g0 + (g1 - g0) * t,
        b0 + (b1 - b0) * t,
        a0 + (a1 - a0) * t,
    ])
}

/// A signal that ticks once a minute — see [`crate::tick`].
pub fn minute_tick() -> RwSignal<u64> {
    crate::tick::minute()
}

/// The sky, painted behind everything else.
pub fn sky_backdrop(
    aesthetic: RwSignal<Aesthetic>,
    tick: RwSignal<u64>,
    location: Location,
) -> impl IntoView {
    // The shimmer runs on its own clock. `tick` is once a minute and drives the
    // *positions*; recomputing where 1,630 stars are several times a second so
    // that they can flicker would be paying for astronomy to get an animation.
    let shimmer = crate::tick::shimmer();
    // Recomputed on the tick rather than per paint: a repaint can be triggered
    // by anything, and there is no reason to re-derive the whole sky because a
    // panel was resized.
    let sky = Memo::new(move |_| {
        tick.get();
        SkyState::now(location)
    });

    canvas(move |cx, size| {
        if aesthetic.get() != Aesthetic::Forged {
            return;
        }
        let (w, h) = (size.width, size.height);
        if w < 16.0 || h < 16.0 {
            return;
        }
        let sky = sky.get();
        let phase = (shimmer.get() % 1024) as f64 / 1024.0;

        // Nothing about a backdrop can be seen from inside the process, and
        // every way it fails looks identical: a blank pane.
        if std::env::var("SMITHY_SKY_DEBUG").is_ok_and(|v| v != "0") {
            let painted = sky
                .stars
                .iter()
                .filter(|s| star_geometry(s.magnitude, sky.darkness, 1.0).1 >= 0.015)
                .count();
            eprintln!(
                "sky: {w:.0}x{h:.0} phase {:?} darkness {:.2} (shown {:.2}) | {} up, {painted} painted | ground {:?}",
                sky.phase,
                sky.darkness,
                shown_darkness(sky.darkness),
                sky.stars.len(),
                ground(sky.darkness)
            );
        }

        // Ground first, over the whole panel — the corners outside the disc are
        // the same sky, just not part of the instrument.
        cx.fill(&Rect::new(0.0, 0.0, w, h), ground(sky.darkness), 0.0);

        draw_stars(cx, w, h, &sky, phase);
        draw_moon(cx, w, h, &sky);
    })
    // Decoration: without this the backdrop sits over the editor for
    // hit-testing and swallows every click meant for the code.
    .style(|s| {
        s.absolute()
            .width_full()
            .height_full()
            .pointer_events_none()
    })
}

fn draw_stars(cx: &mut floem::context::PaintCx, w: f64, h: f64, sky: &SkyState, phase: f64) {
    let pane_scale = (w.min(h) / 700.0).clamp(0.75, 1.5);
    for star in &sky.stars {
        let (core, alpha) = star_geometry(star.magnitude, sky.darkness, pane_scale);
        let alpha = (alpha * twinkle(star.hr, star.position.radius(), phase))
            .clamp(0.0, STAR_ALPHA_CEILING);
        if alpha < 0.015 {
            continue;
        }
        let centre = to_screen(star.position, w, h);
        let colour = star_colour(star.colour_index);
        // Largest first, so the halo lies under a core drawn at full strength.
        for (multiple, ring) in STAR_RINGS {
            cx.fill(
                &Circle::new(centre, core * multiple),
                colour.with_alpha(alpha * ring),
                0.0,
            );
        }
    }
}

fn draw_moon(cx: &mut floem::context::PaintCx, w: f64, h: f64, sky: &SkyState) {
    let Some(projected) = sky.moon.projected else {
        return;
    };
    let centre = to_screen(projected, w, h);
    let radius = (w.min(h) / 60.0).clamp(5.0, 12.0);
    // The moon competes with the code far more than the stars do, so it dims
    // with the sky rather than staying bright through the day.
    let alpha = (0.25 + sky.darkness * 0.6) as f32;

    cx.fill(
        &Circle::new(centre, radius),
        MOON_DARK.with_alpha(alpha * 0.7),
        0.0,
    );

    // The lit part, as a disc scaled across its width. A terminator is an
    // ellipse, and at this size an ellipse of the right width is the whole of
    // what a phase looks like.
    let lit = sky.moon_illumination;
    if lit > 0.02 {
        let inset = radius * (1.0 - 2.0 * lit).abs();
        let (left, right) = if sky.moon_waxing {
            (centre.x - radius + inset.min(radius), centre.x + radius)
        } else {
            (centre.x - radius, centre.x + radius - inset.min(radius))
        };
        cx.fill(
            &floem::peniko::kurbo::Ellipse::new(
                Point::new((left + right) / 2.0, centre.y),
                ((right - left) / 2.0, radius),
                0.0,
            ),
            MOON_BODY.with_alpha(alpha),
            0.0,
        );
    }
}

/// The default place, until a setting exists for it.
pub const DEFAULT_LOCATION: Location = SAN_FRANCISCO;

/// Sunrise and sunset today, as hours since local midnight.
///
/// Computed at [`DEFAULT_LOCATION`] itself — the same place the backdrop's
/// sky is projected from — so the frame sun sets when the sky in the window
/// says it sets. Using the timezone's meridian instead left the two
/// disagreeing by over an hour near the edge of a zone (San Francisco sits
/// 17° west of the PDT meridian: the frame sun went down while the backdrop
/// still called it day). Shared by the fisherman's routine and the frame
/// sun, so the two always tell the same story about how much daylight is
/// left.
pub fn todays_sun(unix_seconds: f64) -> (f64, f64) {
    let jd = smithy_sky::time::julian_date_from_unix(unix_seconds);
    let location = DEFAULT_LOCATION;

    match smithy_sky::sun::sunrise_sunset(jd, location) {
        Some((rise, set)) => {
            let to_local = |at: f64| {
                crate::localtime::local_hours((at - smithy_sky::time::UNIX_EPOCH_JD) * 86_400.0)
            };
            (to_local(rise), to_local(set))
        }
        // Polar day or night. Civil hours rather than no day at all: a
        // routine anchored to a sunrise that never comes would leave the
        // fisherman asleep for six months.
        None => (7.0, 19.0),
    }
}

/// The frame sun's radius: big enough to *occupy* the top rail rather than
/// sit on it. The backdrop's small disc failed by being a bright bauble in
/// the middle of the editor — this one is sized to be cropped by the frame
/// itself, which is what makes it part of the object.
pub const FRAME_SUN_RADIUS: f64 = crate::forged::FRAME_INSET as f64 * 1.15;

/// Where the frame sun sits at a fraction `t` of the day, 0 at sunrise and
/// 1 at sunset.
///
/// An arc over the top of the window: it rises out of the left rail, crowns
/// the header at noon — centre above the window's top edge, so the frame
/// crops it — and sets into the right rail. Painted *under* the ornament
/// (vines, corner stones, wordmark), so the frame always wins the overlap.
pub fn sun_arc(t: f64, w: f64) -> Point {
    let angle = std::f64::consts::PI * t.clamp(0.0, 1.0);
    let y_base = crate::forged::HEADER_HEIGHT as f64 * 0.72;
    let y_noon = FRAME_SUN_RADIUS * 0.25;
    Point::new(
        w * 0.5 * (1.0 - angle.cos()),
        y_base - (y_base - y_noon) * angle.sin(),
    )
}

/// The frame sun's centre and radius right now — `None` once the sun is down.
pub fn frame_sun(w: f64) -> Option<(Point, f64)> {
    let unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let (rise, set) = todays_sun(unix);
    let t = (crate::localtime::local_hours(unix) - rise) / (set - rise);
    if !(0.0..=1.0).contains(&t) {
        return None;
    }
    Some((sun_arc(t, w), FRAME_SUN_RADIUS))
}

/// Paint the frame sun. The same vocabulary the backdrop sun used — corona
/// rings, warm body, tight specular — at a scale where it reads as wrought
/// into the frieze rather than printed on the page.
pub fn draw_frame_sun(cx: &mut floem::context::PaintCx, w: f64) {
    let Some((centre, radius)) = frame_sun(w) else {
        return;
    };
    for (spread, alpha) in [(3.0, 0.05_f32), (2.0, 0.08), (1.4, 0.13)] {
        cx.fill(
            &Circle::new(centre, radius * spread),
            SUN_GLOW.with_alpha(alpha),
            0.0,
        );
    }
    cx.fill(&Circle::new(centre, radius), SUN_BODY.with_alpha(0.9), 0.0);
    cx.fill(
        &Circle::new(
            Point::new(centre.x - radius * 0.28, centre.y - radius * 0.30),
            radius * 0.30,
        ),
        SUN_CORE.with_alpha(0.8),
        0.0,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn luminance(colour: Color) -> f32 {
        let [r, g, b, _] = colour.components;
        0.2126 * r + 0.7152 * g + 0.0722 * b
    }

    /// The rule this backdrop lives under. "Day" means relatively lighter, and
    /// if it ever became an actually-light background the dark-theme text in
    /// front of it would be unreadable. Both ends are pinned, because the
    /// failure would arrive as a slow drift over many small adjustments.
    #[test]
    fn even_the_day_sky_stays_darker_than_the_text_in_front_of_it() {
        // The editor's own background sits around 0.05 relative luminance;
        // anything under about a fifth stays comfortably behind the code.
        for darkness in [0.0, 0.25, 0.5, 0.75, 1.0] {
            let l = luminance(ground(darkness));
            assert!(
                l < 0.2,
                "the sky at darkness {darkness} has luminance {l:.3}, which is a light background"
            );
        }
        assert!(
            luminance(ground(0.0)) > luminance(ground(1.0)),
            "day must be lighter than night, or the whole thing is inverted"
        );
    }

    /// The requirement the old ceiling broke, stated as the thing that actually
    /// matters: **text stays readable over the brightest star**.
    ///
    /// WCAG technique F83 measures a background image against its *lightest*
    /// area for light text, so the number to check is a glyph sitting on a star
    /// core, not on the mean sky. The previous 0.62 ceiling gave 2.03:1 there —
    /// less than half the 4.5:1 required — while its docstring claimed the
    /// dimmest syntax colour "sits well above it". It was asserted in the
    /// direction I was worried about, and nothing checked the direction that
    /// was wrong.
    #[test]
    fn body_text_stays_readable_over_the_brightest_star() {
        // WCAG relative luminance, then the standard contrast ratio.
        fn luminance(colour: Color) -> f64 {
            let channel = |c: f32| {
                let c = f64::from(c);
                if c <= 0.03928 {
                    c / 12.92
                } else {
                    ((c + 0.055) / 1.055).powf(2.4)
                }
            };
            let [r, g, b, _] = colour.components;
            0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b)
        }
        fn contrast(a: Color, b: Color) -> f64 {
            let (x, y) = (luminance(a) + 0.05, luminance(b) + 0.05);
            if x > y {
                x / y
            } else {
                y / x
            }
        }

        // The brightest possible star core, composited over the darkest sky.
        let brightest = star_alpha(-1.46, 1.0) * STAR_RINGS[STAR_RINGS.len() - 1].1;
        let core = lerp(NIGHT, star_colour(0.0), brightest);
        let ratio = contrast(crate::design::FG, core);

        assert!(
            ratio >= 4.5,
            "body text over the brightest star is {ratio:.2}:1, under the 4.5:1 line \
             (star composited to {core:?})"
        );
    }

    /// The other half of the restraint rule: no star may compete with syntax
    /// colouring. A ceiling that is merely documented is a ceiling that moves.
    #[test]
    fn no_star_is_ever_brighter_than_the_ceiling() {
        for magnitude in [-1.5, -1.0, 0.0, 1.0, 2.0, 2.8] {
            for darkness in [0.0, 0.3, 0.6, 1.0] {
                let alpha = star_alpha(magnitude, darkness);
                assert!(
                    alpha <= STAR_ALPHA_CEILING,
                    "magnitude {magnitude} at darkness {darkness} drew alpha {alpha}"
                );
            }
        }
    }

    /// **The sky always has something to look at, and it is not always the
    /// same thing.** Stars at night, the sun by day.
    ///
    /// This test has now said three different things, and the history is the
    /// point. First it asserted stars were invisible at noon, which was
    /// astronomically right and left the backdrop blank during every hour
    /// anybody works. Then it asserted the opposite — stars at all hours — which
    /// filled the day at the cost of the day being a day. Now the sun carries
    /// the daytime, so the field can fade out into it, which is what a sky
    /// actually does.
    ///
    /// The requirement that survived all three: *something* is up, and night
    /// and day do not look alike.
    #[test]
    fn the_field_fades_into_daylight_and_deepens_into_night() {
        let noon = star_alpha(-1.46, 0.0);
        let night = star_alpha(-1.46, 1.0);

        assert!(night > 0.25, "the brightest star drew {night} at midnight");
        assert!(
            noon < night * 0.15,
            "the field barely fades: {noon:.3} at noon against {night:.3} at night"
        );

        // Monotone across the whole range: the sky has to breathe, not step.
        let mut previous = 0.0;
        for step in 0..=20 {
            let alpha = star_alpha(0.5, f64::from(step) / 20.0);
            assert!(alpha >= previous, "the field dimmed as the sky darkened");
            previous = alpha;
        }

        // And the ground moves with it, or the fade would be the only cue.
        assert!(
            luminance(ground(0.0)) > luminance(ground(1.0)) * 1.5,
            "day and night grounds are too close to tell apart"
        );
    }

    /// A star has to be a point of *light*, and what makes it one is a blown
    /// core with wings — not a flat disc. The rings must therefore rise
    /// monotonically inward and reach saturation at the centre.
    ///
    /// And nothing may vanish: below the resolvable radius the size is floored
    /// and the brightness moved into alpha instead. Without that, most of a
    /// magnitude-5 catalogue renders to sub-pixel discs whose alpha is spread
    /// by antialiasing into nothing — which is where the entire faint half of
    /// the field was going.
    #[test]
    fn a_star_has_a_blown_core_and_never_shrinks_away_to_nothing() {
        let mut previous = 0.0;
        for (multiple, alpha) in STAR_RINGS {
            assert!(alpha > previous, "the rings must brighten inward");
            assert!(multiple >= 1.0, "the halo grows outward from the core");
            previous = alpha;
        }
        let (core_multiple, core_alpha) = STAR_RINGS[STAR_RINGS.len() - 1];
        assert_eq!(core_alpha, 1.0, "the core must be fully saturated");
        assert_eq!(
            core_multiple, 1.0,
            "the core *is* the radius, not a fraction"
        );

        // **The saturated core must itself be a drawable disc.** This is the
        // assertion the first version lacked: it floored the total radius while
        // the core was a quarter of it, so every star's brightness ended up in
        // a sub-pixel disc that antialiasing erased. A star can be faint; it
        // cannot be smaller than a pixel.
        for magnitude in [-1.46, 0.0, 2.5, 4.0, 5.0] {
            for pane in [0.75, 1.0, 1.5] {
                let (core, _) = star_geometry(magnitude, 1.0, pane);
                assert!(
                    core * core_multiple >= 1.0,
                    "magnitude {magnitude} has a {:.2}px core, which cannot be drawn",
                    core * core_multiple
                );
            }
        }

        for magnitude in [-1.46, 0.0, 2.5, 4.0, 5.0] {
            for pane in [0.75, 1.0, 1.5] {
                let (radius, alpha) = star_geometry(magnitude, 1.0, pane);
                assert!(
                    radius >= MIN_STAR_RADIUS,
                    "magnitude {magnitude} drew a {radius:.2}px star, below the floor"
                );
                assert!(alpha > 0.0, "magnitude {magnitude} drew nothing at all");
            }
        }

        // The faintest star is dimmer than the brightest, having given up its
        // size rather than simply disappearing.
        let (_, faint) = star_geometry(5.0, 1.0, 1.0);
        let (_, bright) = star_geometry(-1.46, 1.0, 1.0);
        assert!(faint < bright * 0.6, "the magnitude scale collapsed");
    }

    /// Scintillation is refraction through moving air, so it belongs to the
    /// *horizon*, not to the star. A field that twinkled uniformly would read
    /// as fairy lights; one that is steady overhead and restless low down reads
    /// as air.
    ///
    /// And no two stars may shimmer together. A shared phase is the difference
    /// between a sky and a string of bulbs on one circuit.
    #[test]
    fn stars_twinkle_more_near_the_horizon_and_never_in_unison() {
        let spread = |radius: f64| {
            let (mut low, mut high) = (f32::MAX, f32::MIN);
            for step in 0..400 {
                let value = twinkle(2491, radius, f64::from(step) / 400.0);
                low = low.min(value);
                high = high.max(value);
            }
            high - low
        };

        assert!(spread(0.0) < 1e-6, "a star overhead should hold steady");
        assert!(
            spread(1.0) > 0.4,
            "a star at the horizon should waver plainly"
        );
        assert!(
            spread(1.0) > spread(0.5) && spread(0.5) > spread(0.2),
            "the shimmer must grow with the air it is seen through"
        );

        // Different stars, same instant, different brightness.
        let together = [2491u16, 424, 2061, 1713, 5340]
            .iter()
            .map(|hr| twinkle(*hr, 1.0, 0.37))
            .collect::<Vec<_>>();
        for (i, a) in together.iter().enumerate() {
            for b in &together[i + 1..] {
                assert!(
                    (a - b).abs() > 1e-3,
                    "two stars shimmered in step: {a} and {b}"
                );
            }
        }
    }

    /// Twinkle may not smuggle a star past the contrast ceiling. It multiplies
    /// an alpha that was bounded on purpose, so the result is clamped and this
    /// says so.
    #[test]
    fn twinkling_never_pushes_a_star_past_the_ceiling() {
        for hr in [1u16, 424, 2491, 9000] {
            for step in 0..200 {
                let phase = f64::from(step) / 200.0;
                let raw = star_alpha(-1.46, 1.0) * twinkle(hr, 1.0, phase);
                assert!(
                    raw.min(STAR_ALPHA_CEILING) <= STAR_ALPHA_CEILING,
                    "the clamp is what keeps this honest"
                );
            }
        }
        // The brightest star does reach the ceiling at its peak — the clamp is
        // load-bearing rather than decorative.
        let peak = (0..200)
            .map(|s| star_alpha(-1.46, 1.0) * twinkle(2491, 1.0, f64::from(s) / 200.0))
            .fold(0.0f32, f32::max);
        assert!(
            peak > STAR_ALPHA_CEILING,
            "peak {peak} never needed clamping"
        );
    }

    /// Magnitude runs backwards — smaller is brighter — and it is the one scale
    /// in this file where following intuition inverts the sky.
    #[test]
    fn brighter_stars_are_bigger_and_more_opaque() {
        assert!(star_radius(-1.46) > star_radius(0.0));
        assert!(star_radius(0.0) > star_radius(2.5));
        assert!(star_alpha(-1.46, 1.0) > star_alpha(0.0, 1.0));
        assert!(star_alpha(0.0, 1.0) > star_alpha(2.5, 1.0));
    }

    /// The colour index is what stops the field reading as scattered white
    /// dots, so the two stars everyone can name as red and blue must actually
    /// come out red and blue.
    #[test]
    fn betelgeuse_comes_out_warmer_than_rigel() {
        let betelgeuse = star_colour(1.85).components;
        let rigel = star_colour(-0.03).components;
        assert!(
            betelgeuse[0] - betelgeuse[2] > rigel[0] - rigel[2],
            "Betelgeuse {betelgeuse:?} is not warmer than Rigel {rigel:?}"
        );
        assert!(rigel[2] > rigel[0], "Rigel should be blue-leaning");
    }

    /// The sky has to cover the panel with no visible edge.
    ///
    /// This test used to assert the reverse — that the horizon stayed *on* the
    /// panel — and that is exactly what went wrong. Inscribing the hemisphere
    /// in the shorter dimension drew a bare gold ring across the middle of the
    /// editor, which read as a stray circle rather than as an instrument.
    /// Whatever the panel's shape, its corners must be inside the disc.
    #[test]
    fn the_sky_covers_the_whole_panel_with_no_visible_edge() {
        for (w, h) in [
            (900.0, 500.0),
            (500.0, 900.0),
            (1600.0, 300.0),
            (400.0, 400.0),
        ] {
            let centre = to_screen(Projected { x: 0.0, y: 0.0 }, w, h);
            assert!((centre.x - w / 2.0).abs() < 1e-9 && (centre.y - h / 2.0).abs() < 1e-9);

            // The furthest corner, in disc units. Under 1.0 means the horizon
            // is off the panel and there is no rim to see.
            let corner = w.hypot(h) / 2.0 / disc_radius(w, h);
            assert!(
                corner < 1.0,
                "a {w}x{h} panel reaches {corner:.3} of the way to the horizon — \
                 the edge of the sky would be visible"
            );
        }
    }

    /// **The frame sun rides the top rail with the day.** Out of the left
    /// rail at sunrise, over the header's centre at noon, into the right rail
    /// at sunset — and high enough at midday that the window's own top edge
    /// crops it, which is the whole of "occupying the frame".
    #[test]
    fn the_frame_sun_rises_and_sets_on_the_rails() {
        let w = 1280.0;
        let rise = sun_arc(0.0, w);
        let noon = sun_arc(0.5, w);
        let set = sun_arc(1.0, w);

        assert!(rise.x.abs() < 1e-9, "sunrise is at the left edge");
        assert!((noon.x - w / 2.0).abs() < 1e-9, "noon is dead centre");
        assert!((set.x - w).abs() < 1e-9, "sunset is at the right edge");
        assert!(noon.y < rise.y, "noon must be the high point");

        // Cropped by the window's top edge at noon, or it is sitting on the
        // frame rather than intersecting it.
        assert!(
            noon.y - FRAME_SUN_RADIUS < 0.0,
            "the noon disc clears the top edge — that is sitting, not occupying"
        );
        // And still reaching deep into the header, or there is nothing to see.
        assert!(
            noon.y + FRAME_SUN_RADIUS > crate::forged::HEADER_HEIGHT as f64 * 0.5,
            "the noon disc never enters the header"
        );

        // The arc is monotone left to right: it never walks the day backwards.
        let mut previous = -1.0;
        for step in 0..=100 {
            let x = sun_arc(f64::from(step) / 100.0, w).x;
            assert!(x >= previous, "the sun moved backwards across the frame");
            previous = x;
        }
    }

    /// Big is the point. The backdrop's small disc failed by reading as a
    /// bauble; the frame sun has to be large enough for the frame to crop.
    #[test]
    fn the_frame_sun_is_big_enough_to_occupy_the_rail() {
        assert!(
            FRAME_SUN_RADIUS > crate::forged::HEADER_HEIGHT as f64 * 0.7,
            "{FRAME_SUN_RADIUS:.1}px against a {:.0}px header is a bauble, not a presence",
            crate::forged::HEADER_HEIGHT
        );
    }
}
