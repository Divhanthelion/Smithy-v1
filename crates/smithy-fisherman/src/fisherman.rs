//! The fisherman, drawn.
//!
//! *What* he is doing at any moment is [`crate::routine`] — a scripted day
//! anchored to the real sun. This file is the other half: what each of those
//! looks like, and where on the rail it happens.
//!
//! The split is the same one that has worked everywhere else here. A day can be
//! checked in a millisecond; a silhouette cannot be checked at all, so as
//! little as possible lives on that side of the line.

/// Where each place sits along the rail, as a fraction of the usable span.
///
/// The hut is at the left end and the water at the right, so his day reads
/// left-to-right: he comes out of the door, crosses his garden, and walks to
/// the end of the rail to fish.
pub fn place_position(place: Place) -> f64 {
    match place {
        Place::Hut => 0.0,
        Place::Doorstep => 0.10,
        Place::Garden => 0.22,
        Place::Fire => 0.16,
        Place::Perch => 1.0,
    }
}

/// Which way he is facing: `1.0` right, `-1.0` left.
///
/// The poses are all drawn facing right, so anything moving left has to be
/// mirrored. Without this he moonwalks — which is exactly what he did while
/// building the hut, since the lumber is to his right and the wall to his left,
/// so every loaded trip was made backwards.
pub fn facing(from: f64, to: f64) -> f64 {
    if to < from - 1e-6 {
        -1.0
    } else {
        1.0
    }
}

/// Where he is along the rail, walking included.
///
/// A walk eases from where he was to where he is going, so he arrives rather
/// than appears — `routine` guarantees a walk block in front of every change of
/// place, and this is what makes it visible.
pub fn position_along(previous: Place, block_place: Place, doing: Doing, progress: f64) -> f64 {
    if doing == Doing::Walking {
        let from = place_position(previous);
        let to = place_position(block_place);
        // He arrives *before* the walk ends and stands there for the rest of
        // it. That beat is the whole difference between going inside and being
        // deleted — Stardew sells the same transition with four frames of door
        // and a cut, and the hold is what makes the cut read as a decision.
        return from + (to - from) * ease((progress / ARRIVAL).min(1.0));
    }
    place_position(block_place)
}

/// The last stretch of the build, spent walking to wherever his day has him.
///
/// Without it he snapped from the wall to his day's place the instant the hut
/// was finished — and if that place was indoors, he snapped straight out of
/// existence. The handover has to be a walk like any other, or the first thing
/// that happens after the hut goes up is the thing this whole file exists to
/// avoid.
pub const HANDOVER: f64 = 0.90;

/// How much of a walk is spent moving. The rest is the beat at the door.
///
/// Roughly a step's worth of pause at the end, scaled off the walk's length.
pub const ARRIVAL: f64 = 0.80;

/// How far the door stands open, 0 to 1.
///
/// Opens under him as he arrives and swings shut behind him as he leaves, which
/// is the only thing on screen that says he went *through* it rather than
/// ceasing to exist at the wall.
pub fn door_openness(doing: Doing, place: Place, previous: Place, progress: f64) -> f64 {
    if doing != Doing::Walking {
        return 0.0;
    }
    if place == Place::Hut {
        // Arriving: it opens during the beat.
        return ease(((progress - ARRIVAL) / (1.0 - ARRIVAL)).clamp(0.0, 1.0));
    }
    if previous == Place::Hut {
        // Leaving: open as he steps out, shut behind him.
        return 1.0 - ease((progress / 0.30).clamp(0.0, 1.0));
    }
    0.0
}

/// How much lamplight spills through the doorway, 0 to ~0.6.
///
/// A function of both the lamp and the door rather than a constant, so a shut
/// door never glows and a dark room never spills — the two cues can't
/// contradict each other.
pub fn door_glow(lit: f64, door_open: f64) -> f64 {
    0.6 * lit.clamp(0.0, 1.0) * ease(door_open)
}

/// A cheap deterministic mixer, so "random" is a pure function of its inputs.
///
/// splitmix64's finaliser. No dependency, and the same seed gives the same day
/// on every machine — which is what lets the behaviour be tested at all.
fn mix(a: u64, b: u64) -> u64 {
    let mut z = a
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add(b.wrapping_mul(0xBF58_476D_1CE4_E5B9));
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// A deterministic fraction in `0.0..1.0` from two numbers.
///
/// Shared with [`crate::routine`], which needs the same "random but fixed"
/// property for the same reason: a day has to play out identically every time
/// it is asked about, or he changes his mind while being watched.
pub fn unit_from(a: i64, b: i64) -> f64 {
    unit_interval(mix(a as u64, b as u64))
}

/// A mixed value as a fraction in `0.0..1.0`.
fn unit_interval(value: u64) -> f64 {
    (value >> 11) as f64 / (1u64 << 53) as f64
}

// ---------------------------------------------------------------------------
// Drawing
// ---------------------------------------------------------------------------

use kurbo::{BezPath, Circle, Ellipse, Point, Rect, Shape, Vec2};
use peniko::Color;

use crate::ink::{Ink, Part};
use crate::routine::{Doing, Place};

/// Flatten a kurbo shape to a path for [`Ink`].
///
/// Tolerance 0.25: he is a few dozen pixels tall on the rail, and a tighter
/// flatten only thickened the path lists without changing a pixel that mattered.
fn shape_path(shape: &impl Shape) -> BezPath {
    BezPath::from_vec(shape.path_elements(0.25).collect())
}

/// Dark metal, in the frame's own vocabulary. He is a wrought figure someone
/// set there, not a cartoon pasted on: a cartoon on an engraved steel object
/// reads as a sticker, a weathervane reads as part of the object.
pub const IRON: Color = Color::from_rgb8(17, 20, 27);
/// The gold rim light, the frame's inlay.
pub const RIM: Color = Color::from_rgb8(186, 148, 72);
pub const RIM_BRIGHT: Color = Color::from_rgb8(240, 210, 140);
/// The line, visible against the moulding without competing with it.
pub const LINE: Color = Color::from_rgb8(126, 138, 162);
pub const FIRE_CORE: Color = Color::from_rgb8(255, 224, 150);
pub const FIRE_BODY: Color = Color::from_rgb8(226, 132, 44);
pub const FIRE_DEEP: Color = Color::from_rgb8(126, 48, 18);
pub const FISH: Color = Color::from_rgb8(150, 172, 196);
pub const SMOKE: Color = Color::from_rgb8(150, 158, 172);
/// Paper, the one genuinely pale thing on him — which is why the book reads.
pub const GREEN: Color = Color::from_rgb8(96, 138, 84);
/// Paper, the one genuinely pale thing he owns.
pub const PAGE: Color = Color::from_rgb8(196, 190, 172);

/// A figure, in a unit box: `0,0` top-left, `1,1` bottom-right, facing right.
///
/// Normalised so he scales to whatever band he is given, and so "he stays
/// inside his own box" is a property that can be asserted rather than eyeballed.
///
/// Joints rather than outlines, because the outline is *derived* from them —
/// limbs are tapered quads between joints and the body is a tapered trunk. That
/// is what makes pose interpolation possible at all: interpolating six points
/// is easy, interpolating two silhouettes is not.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pose {
    pub head: Point,
    pub head_radius: f64,
    pub shoulder: Point,
    pub elbow: Point,
    pub hip: Point,
    pub knee: Point,
    pub foot: Point,
    pub hand: Point,
    pub rod_tip: Point,
    /// Where the rod is held or planted. Not always the hand — leaned against
    /// something while he reads, laid down while he cooks.
    pub rod_butt: Point,
    /// How far the hat brim tips, as a fraction of its width. He is looking
    /// down at the water when seated and up when he stands.
    pub hat_tilt: f64,
}

/// Measured off a reference silhouette rather than invented, which is why the
/// numbers are not round. Normalised to the figure's height, hat crown to boot
/// sole, with the origin at the back of the body.
///
/// Two things came out of that measurement and neither was guessable. The rod
/// reaches **1.58 figure-heights** to the right — the long shallow diagonal is
/// most of the silhouette, and the previous 0.97 made him look like a man
/// holding a stick. And the body's mass is almost all *behind* the joint line:
/// 0.21 back against 0.10 front. The hunch is the signature.
/// **Cross-legged, on the ground.** The first version put the hip at 0.64 with
/// the knee below and the foot further forward still, which is a lunge: seen
/// from the side he read as crouching to spring rather than settling in for the
/// afternoon.
///
/// What makes it read as crossed rather than merely low is the *shin folding
/// back*. The knee goes forward and outward, and the foot returns toward the
/// body instead of continuing away from it — so the near leg is a shallow
/// triangle rather than a diagonal. The far leg is derived by mirroring the
/// stride vector about the hip (see `figure_strokes`), and because that vector
/// is now short and mostly horizontal, the far leg folds under him too.
///
/// The whole torso drops with the hip. He therefore occupies less of the unit
/// box than a standing pose, which is correct — a seated man is shorter than a
/// standing one, and the poses share one box precisely so that stays true.
const SEATED: Pose = Pose {
    head: Point::new(0.29, 0.26),
    head_radius: 0.086,
    shoulder: Point::new(0.29, 0.42),
    elbow: Point::new(0.39, 0.56),
    hip: Point::new(0.20, 0.76),
    // Forward and out…
    knee: Point::new(0.52, 0.85),
    // …and the shin folds back under him. This is the whole cue.
    foot: Point::new(0.30, 0.93),
    hand: Point::new(0.56, 0.62),
    rod_tip: Point::new(1.58, 0.42),
    rod_butt: Point::new(0.48, 0.65),
    hat_tilt: 0.16,
};

const STANDING: Pose = Pose {
    head: Point::new(0.30, 0.11),
    head_radius: 0.086,
    shoulder: Point::new(0.31, 0.27),
    // The arm bends up to grip the rod where it rests on the shoulder — a
    // shouldered carry reads as a saunter; a rod floating across the chest
    // with the arm hanging reads as a spear levelled at nobody.
    elbow: Point::new(0.40, 0.36),
    hip: Point::new(0.30, 0.56),
    knee: Point::new(0.31, 0.76),
    foot: Point::new(0.33, 0.95),
    hand: Point::new(0.46, 0.32),
    // Shouldered, pointing back and up — the saunter silhouette. The butt is
    // placed so the shaft passes *through* the shoulder point: it rests on
    // him, rather than crossing his neck (its first home) or floating in
    // front of his chest.
    rod_tip: Point::new(-0.62, 0.06),
    rod_butt: Point::new(0.58, 0.33),
    hat_tilt: -0.05,
};

const STRIDE_FORWARD: Pose = Pose {
    foot: Point::new(0.55, 0.94),
    knee: Point::new(0.49, 0.75),
    ..STANDING
};

const STRIDE_BACK: Pose = Pose {
    foot: Point::new(0.23, 0.94),
    knee: Point::new(0.31, 0.77),
    ..STANDING
};

const CURLED: Pose = Pose {
    head: Point::new(0.24, 0.70),
    head_radius: 0.086,
    shoulder: Point::new(0.36, 0.75),
    elbow: Point::new(0.46, 0.82),
    hip: Point::new(0.56, 0.79),
    knee: Point::new(0.70, 0.83),
    foot: Point::new(0.63, 0.93),
    hand: Point::new(0.40, 0.86),
    // Laid flat beside him.
    rod_tip: Point::new(1.30, 0.97),
    rod_butt: Point::new(0.10, 0.97),
    hat_tilt: 0.30,
};

/// Crouched over the fire, holding a stick out to it.
const CROUCHED: Pose = Pose {
    head: Point::new(0.28, 0.36),
    head_radius: 0.086,
    shoulder: Point::new(0.30, 0.50),
    elbow: Point::new(0.42, 0.58),
    hip: Point::new(0.24, 0.70),
    knee: Point::new(0.44, 0.74),
    foot: Point::new(0.34, 0.92),
    hand: Point::new(0.56, 0.60),
    // Set down behind him while he cooks.
    rod_tip: Point::new(1.10, 0.97),
    rod_butt: Point::new(-0.05, 0.97),
    hat_tilt: 0.22,
};

/// Seated, hand up to the mouth.
const FEASTING: Pose = Pose {
    hand: Point::new(0.40, 0.41),
    elbow: Point::new(0.42, 0.59),
    rod_tip: Point::new(1.10, 0.97),
    rod_butt: Point::new(-0.05, 0.97),
    ..SEATED
};

/// Cigarette to the lips.
const PUFFING: Pose = Pose {
    hand: Point::new(0.39, 0.33),
    elbow: Point::new(0.42, 0.53),
    ..SEATED
};

/// Rod leaned aside, book open on the knee, head down.
const READING_POSE: Pose = Pose {
    head: Point::new(0.30, 0.29),
    shoulder: Point::new(0.30, 0.43),
    elbow: Point::new(0.38, 0.57),
    // The book rests on the crossed knee, which is now much lower.
    hand: Point::new(0.46, 0.62),
    // Leaned back against the frame, butt on the ground behind him.
    rod_tip: Point::new(-0.34, 0.04),
    rod_butt: Point::new(0.22, 0.96),
    hat_tilt: 0.30,
    ..SEATED
};

/// Standing, arms up, back arched.
///
/// The reach has to **clear the head's silhouette** or it is invisible: the
/// first version put the elbow at (0.28, 0.10) — inside the head circle — and
/// the hand a hair above the crown, so the arm rose straight up *behind* the
/// head and the stretch read as a man simply standing there. Elbow out and
/// back, hand well above the crown, and the yawn-stretch reads.
const STRETCHED: Pose = Pose {
    head: Point::new(0.29, 0.13),
    elbow: Point::new(0.19, 0.07),
    hand: Point::new(0.24, -0.13),
    rod_tip: Point::new(1.30, 0.97),
    rod_butt: Point::new(0.10, 0.97),
    hat_tilt: -0.16,
    ..STANDING
};

fn lerp_point(from: Point, to: Point, t: f64) -> Point {
    Point::new(from.x + (to.x - from.x) * t, from.y + (to.y - from.y) * t)
}

/// Blend two poses. Animation is interpolation between a handful of key poses,
/// not a sprite sheet — so it stays crisp at any size and a saunter can be slow
/// without needing more frames.
pub fn blend(from: Pose, to: Pose, t: f64) -> Pose {
    let t = t.clamp(0.0, 1.0);
    Pose {
        head: lerp_point(from.head, to.head, t),
        head_radius: from.head_radius + (to.head_radius - from.head_radius) * t,
        shoulder: lerp_point(from.shoulder, to.shoulder, t),
        elbow: lerp_point(from.elbow, to.elbow, t),
        hip: lerp_point(from.hip, to.hip, t),
        knee: lerp_point(from.knee, to.knee, t),
        foot: lerp_point(from.foot, to.foot, t),
        hand: lerp_point(from.hand, to.hand, t),
        rod_tip: lerp_point(from.rod_tip, to.rod_tip, t),
        rod_butt: lerp_point(from.rod_butt, to.rod_butt, t),
        hat_tilt: from.hat_tilt + (to.hat_tilt - from.hat_tilt) * t,
    }
}

/// Smoothstep, so nothing starts or stops abruptly.
fn ease(t: f64) -> f64 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// A there-and-back ease, for a movement that returns to where it began.
fn pulse(t: f64) -> f64 {
    let t = t.clamp(0.0, 1.0);
    ease(1.0 - (t * 2.0 - 1.0).abs())
}

/// How many full gait cycles he makes crossing between two places.
///
/// A cycle is two footfalls — back to forward to back — so the cycles are
/// half the steps. Derived from the walk's length and a real cadence rather
/// than picked, so the two can never drift apart — which is exactly how the
/// previous constant came to imply one step every eighteen seconds.
fn strides() -> f64 {
    (crate::routine::WALK_SECONDS / (crate::routine::STEP_SECONDS * 2.0)).max(2.0)
}

/// Secondary motions, in seconds per cycle.
///
/// **Deliberately mutually prime.** Williams: "CYCLES ARE MECHANICAL AND LOOK
/// JUST LIKE WHAT THEY ARE — CYCLES", and his fix is to have the arms and head
/// perform on their own timing rather than the body's. Round numbers phase-lock
/// — 4, 8 and 12 seconds all line up every twelve — and the composite then
/// reads as one big loop, which is the Chuck Jones granddaughter problem:
/// "why does the same wave keep lapping on the island?"
///
/// These recombine on the order of hours instead. It is the same code with
/// different constants and it is the cheapest thing here that buys life.
const BREATH_SECONDS: f64 = 3.7;
const LINE_SWAY_SECONDS: f64 = 5.3;
const HEAD_SECONDS: f64 = 11.3;
const SETTLE_SECONDS: f64 = 17.9;

/// How far he rises and falls with a breath, as a fraction of his height.
///
/// Real vertical travel of the body's centre of mass while walking is 2.7–4.8cm
/// (Orendurff et al., 2004), which on a 1.7m frame is 1.6–2.8% of stature —
/// **sub-pixel at thirty-six pixels tall**. So it is exaggerated, which is what
/// Williams does throughout ("LET'S SPREAD IT OUT AND EXAGGERATE IT A LITTLE
/// MORE SO IT'S CLEARER"). Roughly three times life, which lands at 2px.
const BREATH_RISE: f64 = 0.055;
/// A seated idle breathes less than a walk bobs.
const SEATED_BREATH: f64 = 0.022;

/// How far the rod may reach either side of him, in figure-heights.
///
/// It is exempt from the body's box — see
/// `no_pose_ever_leaves_the_box_he_is_drawn_in` — but not unbounded: the rail
/// has a corner ornament at each end and he must not poke it.
pub const ROD_REACH: f64 = 1.7;

/// Lift the whole figure by `rise`, in figure-heights.
///
/// Applied after the pose so every activity breathes without each one having to
/// remember to — a statue is a figure that holds perfectly still, and nothing
/// alive does.
pub fn breathe(pose: Pose, rise: f64) -> Pose {
    let lift = |p: Point| Point::new(p.x, p.y + rise);
    Pose {
        head: lift(pose.head),
        shoulder: lift(pose.shoulder),
        elbow: lift(pose.elbow),
        hip: lift(pose.hip),
        hand: lift(pose.hand),
        // Feet stay on the ground; the body rises off them. Lifting everything
        // together is a figure hopping, which is the tell that a bob was added
        // as an afterthought.
        ..pose
    }
}

/// The breath, and the slow secondary motions, at a moment.
///
/// `seconds` is wall-clock, so the periods above mean what they say.
/// How far his hat tips as he looks about, in the pose's own units.
///
/// Its own period again, and the longest of them: a head that turned on the
/// breath would read as one motion rather than two.
pub fn head_drift(seconds: f64) -> f64 {
    ((seconds / HEAD_SECONDS) * std::f64::consts::TAU).sin() * 0.06
}

pub fn secondary(seconds: f64, doing: Doing) -> f64 {
    let wave = |period: f64| ((seconds / period) * std::f64::consts::TAU).sin();
    let depth = match doing {
        Doing::Walking | Doing::Exercising => BREATH_RISE,
        Doing::Sleeping | Doing::Siesta => SEATED_BREATH * 0.6,
        _ => SEATED_BREATH,
    };
    // Two periods that do not divide each other, so the rise never settles into
    // a beat a viewer can predict.
    (wave(BREATH_SECONDS) * 0.7 + wave(SETTLE_SECONDS) * 0.3) * depth
}

/// The pose for what he is doing.
///
/// Every activity resolves to a blend of a handful of key poses. That is what
/// makes adding one cheap — a new activity needs a line here and, at most, one
/// new pose, rather than a sprite sheet.
pub fn pose_for(doing: Doing, progress: f64, phase: f64) -> Pose {
    match doing {
        Doing::Sleeping | Doing::Siesta => CURLED,

        // Sitting up, swinging his legs out, standing.
        Doing::Waking => blend(CURLED, STANDING, ease(progress)),

        // Down, up, and a reach at the top — the reach is what makes it read
        // as exercise rather than as a man repeatedly sitting down.
        Doing::Exercising => {
            let rep = (phase * 2.4).fract();
            if rep < 0.5 {
                blend(STANDING, CROUCHED, pulse(rep * 2.0))
            } else {
                blend(STANDING, STRETCHED, pulse((rep - 0.5) * 2.0))
            }
        }

        // Seated with a mug, brought up to the mouth now and then.
        Doing::Coffee => blend(SEATED, PUFFING, pulse((phase * 0.5).fract()) * 0.8),

        // Bent over the row, working along it.
        Doing::Gardening => {
            let reach = pulse((phase * 1.1).fract());
            blend(CROUCHED, FEASTING, reach * 0.35)
        }

        Doing::Fishing => SEATED,
        Doing::Smoking => blend(SEATED, PUFFING, pulse((phase * 0.8).fract()) * 0.9),

        // Settling to the fire and rising from it.
        Doing::Cooking => {
            let settle = ease((progress / 0.15).min(1.0));
            let rise = ease(((progress - 0.85) / 0.15).max(0.0));
            blend(blend(STANDING, CROUCHED, settle), STANDING, rise)
        }
        Doing::Eating => blend(SEATED, FEASTING, pulse((phase * 1.6).fract()) * 0.85),
        Doing::Reading => READING_POSE,

        Doing::Walking => {
            let stride = (progress * strides()).fract();
            let swing = if stride < 0.5 {
                stride * 2.0
            } else {
                2.0 - stride * 2.0
            };
            blend(STRIDE_BACK, STRIDE_FORWARD, ease(swing))
        }
    }
}

/// A tapered limb, as a closed quad from `from` to `to`.
///
/// Filled shapes rather than strokes, because at this size the silhouette is
/// the whole of what reads. A figure drawn as constant-width line segments is a
/// stick figure however carefully its joints are placed — which is what the
/// first version was, and it looked like one.
pub fn taper(from: Point, to: Point, width_from: f64, width_to: f64) -> BezPath {
    let along = to - from;
    let length = along.hypot().max(1e-9);
    let across = Vec2::new(-along.y / length, along.x / length);

    let mut path = BezPath::new();
    path.move_to(from + across * (width_from / 2.0));
    path.line_to(to + across * (width_to / 2.0));
    path.line_to(to - across * (width_to / 2.0));
    path.line_to(from - across * (width_from / 2.0));
    path.close_path();
    path
}

/// The hat: the one detail that makes him read as a fisherman rather than as a
/// person-shaped smudge.
///
/// A wide brim is worth more than any amount of anatomy at this size, because
/// it is the only part of the silhouette that is *distinctive* — everything
/// else about a small dark figure is shared with every other small dark figure.
pub fn hat_paths(pose: &Pose) -> (BezPath, BezPath) {
    let brim_width = pose.head_radius * 3.4;
    let centre = Point::new(pose.head.x, pose.head.y - pose.head_radius * 0.55);
    let tip = pose.hat_tilt * brim_width * 0.5;

    // The brim, as a shallow closed curve tipping with his gaze.
    let mut brim = BezPath::new();
    let left = Point::new(centre.x - brim_width * 0.42, centre.y - tip * 0.5);
    let right = Point::new(centre.x + brim_width * 0.58, centre.y + tip * 0.5);
    brim.move_to(left);
    brim.quad_to(
        Point::new(centre.x, centre.y + pose.head_radius * 0.55),
        right,
    );
    brim.quad_to(
        Point::new(centre.x, centre.y - pose.head_radius * 0.18),
        left,
    );
    brim.close_path();

    // The crown, a squat dome above it.
    let crown = BezPath::from_vec(
        Ellipse::new(
            Point::new(centre.x + tip * 0.4, centre.y - pose.head_radius * 0.62),
            (pose.head_radius * 0.95, pose.head_radius * 0.85),
            0.0,
        )
        .path_elements(0.05)
        .collect(),
    );

    (brim, crown)
}

/// Every filled piece of the figure, back to front.
pub fn figure_paths(pose: &Pose) -> Vec<BezPath> {
    let r = pose.head_radius;
    let (brim, crown) = hat_paths(pose);

    // **Two legs.** He had one, which reads as a flamingo rather than as a man
    // — and worse, a walk cycle with a single leg has nothing to alternate, so
    // the stride was invisible and the whole thing looked like sliding.
    //
    // The far leg trails the near one by half a stride and sits a little
    // behind, which is the whole of the depth cue at this size.
    let trail = Point::new(
        pose.hip.x - (pose.foot.x - pose.hip.x) * 0.55,
        pose.hip.y + (pose.knee.y - pose.hip.y) * 0.92,
    );
    let far_foot = Point::new(pose.hip.x - (pose.foot.x - pose.hip.x) * 0.75, pose.foot.y);

    vec![
        // Far leg first, so the near one overlaps it.
        taper(pose.hip, trail, r * 1.3, r * 1.0),
        taper(trail, far_foot, r * 1.0, r * 0.7),
        // Near leg.
        taper(pose.hip, pose.knee, r * 1.5, r * 1.2),
        taper(pose.knee, pose.foot, r * 1.2, r * 0.8),
        // Trunk, hip to shoulder.
        taper(pose.hip, pose.shoulder, r * 2.1, r * 1.7),
        // Arm.
        taper(pose.shoulder, pose.elbow, r * 0.9, r * 0.8),
        taper(pose.elbow, pose.hand, r * 0.8, r * 0.6),
        BezPath::from_vec(Circle::new(pose.head, r).path_elements(0.05).collect()),
        crown,
        brim,
    ]
}

/// The line, hanging from the rod tip.
///
/// A catenary rather than a straight drop, and it lags his motion slightly.
/// That one detail is most of what makes him feel alive rather than articulated.
///
/// **It never hangs below `depth`.** That is the whole of "never over text", and
/// it is asserted rather than assumed.
pub fn line_path(tip: Point, depth: f64, sway: f64) -> BezPath {
    let mut path = BezPath::new();
    if depth <= tip.y {
        return path;
    }
    let drop = depth - tip.y;
    let foot = Point::new(tip.x + sway, depth);

    path.move_to(tip);
    path.curve_to(
        Point::new(tip.x, tip.y + drop * 0.45),
        Point::new(tip.x + sway * 0.7, tip.y + drop * 0.8),
        foot,
    );
    path
}

/// The stage along the rail: his scale, where it starts, and how far it runs.
///
/// Shared with the preview harness, because a layout that exists in two
/// places is a layout that drifts — and it already has.
pub fn stage_layout(w: f64, band: f64) -> (f64, f64, f64) {
    let scale = band * 0.80;
    // Clear of the corner ornament: the volutes reach about `band * 1.6`
    // along the rail (the clearance the vines keep), and the hut stands
    // `scale * 0.35` to the left of the stage — so anything less than this
    // and the hut grows out of the corner stone.
    let left = band * 2.1;
    let width = (w - left - band * 1.5 - scale * (1.0 + ROD_REACH)).max(1.0);
    (scale, left, width)
}

/// Everything a frame needs. No clock, no globals — a value.
///
/// The Aesthetic gate and the tiny-window early return live in
/// [`fisherman_view`], not here: `paint` draws whatever Scene it is handed so
/// the harness can render sizes the app refuses.
pub struct Scene {
    pub width: f64,
    pub height: f64,
    pub band: f64,
    pub doing: Doing,
    pub place: Place,
    pub previous: Place,
    pub progress: f64,
    pub completion: f64,
    pub frame: u64,
    pub seconds: f64,
}

/// The clock half. Everything nondeterministic lives in the arguments and
/// nowhere else — `launched` is a parameter rather than `session_seconds()`
/// because that OnceLock cannot be reset, and two scenes in one process would
/// otherwise share a launch time (the preview's whole reason for existing).
#[allow(clippy::too_many_arguments)]
pub fn scene_at(
    width: f64,
    height: f64,
    band: f64,
    hours: f64,
    sunrise: f64,
    sunset: f64,
    day: i64,
    launched: f64,
    frame: u64,
) -> Scene {
    let (block, progress) = crate::routine::at(hours, sunrise, sunset, day);
    let previous = crate::routine::at(block.start - 1e-4, sunrise, sunset, day)
        .0
        .place;
    if std::env::var("SMITHY_FISHERMAN_DEBUG").is_ok_and(|v| v != "0") {
        eprintln!(
            "fisherman: {width:.0}x{height:.0} band {band:.0} | {hours:.2}h day {day} | \
             sun {sunrise:.2}..{sunset:.2} | {:?} at {:?} {:.0}% | indoors {}",
            block.doing,
            block.place,
            progress * 100.0,
            block.doing.is_indoors(block.place)
        );
    }
    Scene {
        width,
        height,
        band,
        doing: block.doing,
        place: block.place,
        previous,
        progress,
        completion: hut_completion(launched),
        frame,
        // Wall-clock seconds of animation phase, same as the live view's
        // `frame as f64 / 5.0` — kept on Scene so paint never reads a clock.
        seconds: frame as f64 / 5.0,
    }
}

/// The drawing half. Pure: same Scene, same ink calls, forever.
pub fn paint(ink: &mut impl Ink, scene: &Scene) {
    let (w, h, band) = (scene.width, scene.height, scene.band);
    let frame = scene.frame;
    let seconds = scene.seconds;
    let phase = seconds / 6.0;
    let progress = scene.progress;
    let completion = scene.completion;
    let came_from = scene.previous;
    let block_doing = scene.doing;
    let block_place = scene.place;

    let (scale, stage_left, stage) = stage_layout(w, band);
    let top = h - band + (band - scale) * 0.55;

    let hut = HutGeometry::new(
        stage_left - scale * 0.35,
        h - band * 0.10,
        scale * 1.45,
        band,
    );
    // While he is walking home at the end of the build, the door answers to
    // that walk rather than to the routine.
    let door_open = if completion < 1.0 {
        let handover = ((completion - HANDOVER) / (1.0 - HANDOVER)).clamp(0.0, 1.0);
        door_openness(Doing::Walking, block_place, Place::Garden, handover)
    } else {
        door_openness(block_doing, block_place, came_from, progress)
    };
    // A stand-in Block for the draw helpers that still take one — same fields
    // paint already has on Scene, without dragging routine into every call.
    let block = crate::routine::Block {
        doing: block_doing,
        place: block_place,
        start: 0.0,
        end: 1.0,
    };
    draw_hut(ink, &hut, &block, progress, frame, completion, door_open);

    // While the hut is going up he is building it, whatever the clock
    // says. He does not go to bed halfway through raising a wall.
    let building = completion < 1.0;
    let (doing, along) = position_for(came_from, block_place, block_doing, progress, completion);
    let face = face_for(came_from, block_place, block_doing, progress, completion);
    let left = stage_left + along * stage;
    // Mirrored about his own centre when he is heading left, so the rod
    // and the plank swap sides with him rather than staying put.
    let at = |p: Point| {
        let x = if face < 0.0 { 1.0 - p.x } else { p.x };
        Point::new(left + x * scale, top + p.y * scale)
    };
    // A plank-carrying walk is just a walk with the arms out, at the
    // walk's own cadence: `pose_for` scales its input by `strides()`, so
    // the span here is the build's duration measured in walk-lengths.
    let walk_progress = if building {
        completion * (BUILD_SECONDS * HANDOVER / crate::routine::WALK_SECONDS)
    } else {
        progress
    };
    let mut pose = breathe(
        pose_for(doing, walk_progress, phase),
        secondary(seconds, doing),
    );
    pose.hat_tilt += head_drift(seconds);

    // Indoors he is a shape behind glass and nothing else — see
    // `draw_hut`, which paints him into the window.
    if !building && block_doing.is_indoors(block_place) {
        return;
    }

    if building {
        draw_figure(ink, &pose, &at, scale);
        draw_plank(ink, &pose, &at, scale, completion);
    } else {
        // The fire is a fixture at the pit, not a prop that follows him —
        // a hearth that teleports to the doorstep for dinner reads as a
        // decal, the same failure as flames without a glow.
        let fire_base = Point::new(
            stage_left + place_position(Place::Fire) * stage + scale * 0.80,
            top + scale * 0.92,
        );
        draw_fire(ink, &block, progress, fire_base, scale, frame);
        draw_line_and_rod(ink, &block, &pose, &at, scale, h, frame);
        draw_figure(ink, &pose, &at, scale);
        draw_props(ink, &block, progress, &pose, &at, scale, frame);
    }
}

/// Where he is along the stage, and what pose drives him, for this frame.
///
/// Split out of [`paint`] so the facing lookback can be tested without a
/// renderer — the lookback used to reach into the previous routine block via
/// a wall-clock second, and that is exactly the behaviour this branch changed.
pub fn position_for(
    previous: Place,
    place: Place,
    doing: Doing,
    progress: f64,
    completion: f64,
) -> (Doing, f64) {
    let building = completion < 1.0;
    if building && completion < HANDOVER {
        (Doing::Walking, build_position(completion))
    } else if building {
        // Finished building; now walk to wherever the day has him, so the
        // handover is a walk rather than a jump.
        let handover = ((completion - HANDOVER) / (1.0 - HANDOVER)).clamp(0.0, 1.0);
        let from = build_position(HANDOVER);
        let to = place_position(place);
        (
            Doing::Walking,
            from + (to - from) * ease((handover / ARRIVAL).min(1.0)),
        )
    } else {
        (doing, position_along(previous, place, doing, progress))
    }
}

/// Which way he faces this frame.
///
/// Within-block lookback rather than a wall-clock second: the old lookback
/// needed the sun and the day inside [`Scene`], which forced every harness
/// tile to invent a coherent clock. The facing *sign* is what matters on a
/// monotonic stretch; at the start of a settled activity the lookback clamps
/// to the block's own progress, so he does not inherit the walk that
/// delivered him (see the test that guards this).
///
/// Build trips are the same idea with a sharper edge. Each plank trip is a
/// there-and-back; subtracting a fixed completion delta across a trip
/// boundary lands in the *previous* trip's inbound, so he walks out loaded
/// while still facing the empty return — the moonwalk `does_not_moonwalk`
/// counted at 29 frames. Clamp the lookback to the current trip's start.
///
/// Handover is the opposite seam: looking *into* the last build trip is
/// what keeps him facing the way he was going until he actually turns for
/// the walk home. Clamping handover lookback to `HANDOVER` made stillness
/// read as facing right at completion 0.9000 while the last outbound step
/// was still landing — a second, distinct moonwalk.
pub fn face_for(
    previous: Place,
    place: Place,
    doing: Doing,
    progress: f64,
    completion: f64,
) -> f64 {
    let building = completion < 1.0;
    let (doing_now, along) = position_for(previous, place, doing, progress, completion);
    let a_moment_ago = if building && completion < HANDOVER {
        // Stay inside this plank trip. Crossing into the previous one is
        // exactly the loaded-trip moonwalk.
        let trip_start = (completion * BUILD_TRIPS).floor() / BUILD_TRIPS;
        build_position((completion - 0.004).max(trip_start))
    } else if building {
        let lookback_c = completion - 0.004;
        if lookback_c < HANDOVER {
            // Still see the last outbound — do not invent a turn until the
            // handover walk itself moves him.
            build_position(lookback_c.max(0.0))
        } else {
            let handover = ((lookback_c - HANDOVER) / (1.0 - HANDOVER)).clamp(0.0, 1.0);
            let from = build_position(HANDOVER);
            from + (place_position(place) - from) * ease((handover / ARRIVAL).min(1.0))
        }
    } else {
        // Clamped: progress 0 stays inside this block. The old code subtracted
        // a wall-clock second and asked the routine, which at a block boundary
        // could answer with the walk that brought him here.
        position_along(previous, place, doing, (progress - 0.004).max(0.0))
    };
    let face = facing(a_moment_ago, along);
    // During the beat at a walk's end he is standing still, and stillness
    // reads as facing right — which at the hut is *away* from the door
    // opening beside him. A stopped walker keeps the walk's own heading.
    if !building && doing_now == Doing::Walking && (along - a_moment_ago).abs() < 1e-6 {
        facing(place_position(previous), place_position(place))
    } else {
        face
    }
}

fn draw_figure(
    ink: &mut impl Ink,
    pose: &Pose,
    at: &impl Fn(Point) -> Point,
    scale: f64,
) {
    ink.begin(Part::Figure);
    let edge = (scale * 0.035).max(0.5);
    for path in figure_paths(pose) {
        let placed = place(&path, at);
        // Dark body, gold edge — the frame's own treatment, so he belongs to
        // the same object rather than sitting on it.
        ink.fill(&placed, IRON);
        ink.stroke(&placed, RIM.with_alpha(0.85), edge);
    }
}

/// Map a path from the unit box onto the panel.
pub fn place(path: &BezPath, at: &impl Fn(Point) -> Point) -> BezPath {
    let mut out = BezPath::new();
    for element in path.elements() {
        use kurbo::PathEl;
        match *element {
            PathEl::MoveTo(p) => out.move_to(at(p)),
            PathEl::LineTo(p) => out.line_to(at(p)),
            PathEl::QuadTo(a, b) => out.quad_to(at(a), at(b)),
            PathEl::CurveTo(a, b, c) => out.curve_to(at(a), at(b), at(c)),
            PathEl::ClosePath => out.close_path(),
        }
    }
    out
}

fn draw_line_and_rod(
    ink: &mut impl Ink,
    block: &crate::routine::Block,
    pose: &Pose,
    at: &impl Fn(Point) -> Point,
    scale: f64,
    panel_height: f64,
    frame: u64,
) {
    // The rod is in his hands at the water, and shouldered on every walk —
    // that shouldered diagonal is the saunter silhouette the standing pose was
    // measured for, and gating it to the perch left him walking with a bent
    // arm around nothing. Everywhere else it is leaning somewhere and is not
    // worth drawing at this size.
    let at_the_water =
        matches!(block.doing, Doing::Fishing | Doing::Smoking) && block.place == Place::Perch;
    if !at_the_water && block.doing != Doing::Walking {
        return;
    }
    let tip = at(pose.rod_tip);

    let mut rod = BezPath::new();
    rod.move_to(at(pose.rod_butt));
    rod.line_to(tip);
    // Rod is part of the silhouette — same Part as the body, so "he exists"
    // and "right size" see the shouldered/cast rod as him, not as a prop.
    ink.begin(Part::Figure);
    ink.stroke(&rod, IRON, (scale * 0.06).max(1.2));
    ink.stroke(&rod, RIM.with_alpha(0.9), (scale * 0.025).max(0.6));

    if !at_the_water {
        return;
    }

    // The line has its own period, prime against the breath, and it *lags* —
    // that lag is most of what makes him read as alive rather than articulated.
    let seconds = frame as f64 / 5.0;
    let sway = (((seconds - 0.4) / LINE_SWAY_SECONDS) * std::f64::consts::TAU).sin() * scale * 0.08;
    let path = line_path(tip, panel_height - 2.0, sway);
    ink.begin(Part::Line);
    ink.stroke(&path, LINE.with_alpha(0.75), 0.9);
}

/// The cooking fire, burning at its pit.
fn draw_fire(
    ink: &mut impl Ink,
    block: &crate::routine::Block,
    progress: f64,
    base: Point,
    scale: f64,
    frame: u64,
) {
    let strength = match block.doing {
        // Catches, burns, then dies back to the embers he eats by — never
        // simply switched on, and never out between the cooking and the
        // eating, which is what made it pop in at the doorstep.
        Doing::Cooking => {
            ease((progress / 0.12).min(1.0)) * (1.0 - 0.5 * ease(((progress - 0.8) / 0.2).max(0.0)))
        }
        Doing::Eating => 0.5 * (1.0 - ease((progress / 0.5).min(1.0))),
        _ => 0.0,
    };
    if strength < 0.02 {
        return;
    }

    ink.begin(Part::Fire);
    let height = scale * 0.38 * strength;

    // The hearth glow first, under everything: bare flames on cold metal read
    // as a decal, and it is the light on the ground that says *fire*.
    ink.fill(&shape_path(&Ellipse::new(base, (scale * 0.30, scale * 0.09), 0.0)), FIRE_BODY.with_alpha(0.22 * strength as f32));

    for (index, colour, spread) in [
        (0usize, FIRE_DEEP, 1.0),
        (1, FIRE_BODY, 0.66),
        (2, FIRE_CORE, 0.33),
    ] {
        let mut flame = BezPath::new();
        let half = scale * 0.12 * spread;
        let lick = height * (1.0 - index as f64 * 0.22);
        flame.move_to(Point::new(base.x - half, base.y));
        flame.quad_to(
            Point::new(base.x - half * 0.7, base.y - lick * 0.6),
            Point::new(base.x + half * 0.15, base.y - lick),
        );
        flame.quad_to(
            Point::new(base.x + half * 0.9, base.y - lick * 0.5),
            Point::new(base.x + half, base.y),
        );
        flame.close_path();
        ink.fill(&flame, colour.with_alpha(0.92));
    }

    // Sparks, rising off the top of the flame and winking out — a fire that
    // only ever burns downward reads as a lampshade.
    for spark in 0..3 {
        let p = ((frame as f64 / 18.0) + f64::from(spark) * 0.37).fract();
        let wander = (f64::from(spark) - 1.0) * scale * 0.05 + p * scale * 0.03;
        ink.fill(&shape_path(&Circle::new(
                Point::new(base.x + wander, base.y - height * (0.5 + p * 0.9)),
                (scale * 0.018).max(0.4),
            )), FIRE_CORE.with_alpha((1.0 - p) as f32 * 0.8 * strength as f32));
    }
}

/// Whatever he happens to be holding.
fn draw_props(
    ink: &mut impl Ink,
    block: &crate::routine::Block,
    progress: f64,
    pose: &Pose,
    at: &impl Fn(Point) -> Point,
    scale: f64,
    frame: u64,
) {
    ink.begin(Part::Props);
    match block.doing {
        Doing::Cooking => {
            // A fish on a stick, held out over the flame.
            let fish = Point::new(0.74, 0.72);
            draw_fish(ink, at(fish), scale, 1.0);
            let mut stick = BezPath::new();
            stick.move_to(at(pose.hand));
            stick.line_to(at(fish));
            ink.stroke(&stick, RIM_BRIGHT.with_alpha(0.8), (scale * 0.02).max(0.5));
        }
        Doing::Eating => {
            let left = 1.0 - ease(progress);
            if left > 0.15 {
                draw_fish(ink, at(pose.hand), scale, left);
            }
        }
        Doing::Coffee => {
            // The mug, and steam off it — the cheapest possible "this is hot".
            let mug = at(Point::new(pose.hand.x, pose.hand.y));
            ink.begin(Part::Props);
            ink.fill(&shape_path(&Circle::new(mug, scale * 0.055)), PAGE.with_alpha(0.9));
            ink.begin(Part::Smoke);
            for puff in 0..2 {
                let p = ((frame as f64 / 26.0) + f64::from(puff) * 0.5).fract();
                ink.fill(&shape_path(&Circle::new(
                        Point::new(mug.x + p * scale * 0.05, mug.y - p * scale * 0.30),
                        (scale * 0.022).max(0.5),
                    )), SMOKE.with_alpha((1.0 - p) as f32 * 0.48));
            }
        }
        Doing::Smoking => {
            // A cigarette, not a ball of light: the paper stick first, then
            // the ember at its tip, which is where the smoke comes from.
            let butt = at(Point::new(pose.hand.x + 0.015, pose.hand.y - 0.005));
            let tip = at(Point::new(pose.hand.x - 0.055, pose.hand.y - 0.035));
            let mut cigarette = BezPath::new();
            cigarette.move_to(butt);
            cigarette.line_to(tip);
            ink.begin(Part::Props);
            ink.stroke(&cigarette, PAGE.with_alpha(0.9), (scale * 0.018).max(0.6));
            ink.fill(&shape_path(&Circle::new(tip, (scale * 0.028).max(0.7))), FIRE_CORE.with_alpha(0.9));
            ink.begin(Part::Smoke);
            for puff in 0..3 {
                let p = ((frame as f64 / 22.0) + f64::from(puff) * 0.33).fract();
                ink.fill(&shape_path(&Circle::new(
                        Point::new(tip.x + p * scale * 0.12, tip.y - p * scale * 0.45),
                        (scale * 0.03).max(0.6) * (1.0 + p * 1.6),
                    )), SMOKE.with_alpha((1.0 - p) as f32 * 0.44));
            }
        }
        Doing::Gardening => {
            // A row of small shoots he is working along.
            for row in 0..4 {
                let x = 0.15 + f64::from(row) * 0.20;
                let mut shoot = BezPath::new();
                let base = at(Point::new(x, 0.95));
                shoot.move_to(base);
                shoot.quad_to(
                    Point::new(base.x - scale * 0.02, base.y - scale * 0.06),
                    Point::new(base.x + scale * 0.03, base.y - scale * 0.10),
                );
                ink.stroke(&shoot, GREEN.with_alpha(0.8), (scale * 0.02).max(0.5));
            }
        }
        _ => {}
    }
}

/// The plank he is carrying, when he has one.
///
/// Only on the way *to* the wall — he walks back empty-handed, which is the
/// detail that makes the trip read as a trip rather than as pacing.
fn draw_plank(
    ink: &mut impl Ink,
    pose: &Pose,
    at: &impl Fn(Point) -> Point,
    scale: f64,
    completion: f64,
) {
    let trip = (completion * BUILD_TRIPS).fract();
    if trip > 0.5 {
        return;
    }
    ink.begin(Part::Props);
    let hand = at(pose.hand);
    let board = Rect::new(
        hand.x - scale * 0.34,
        hand.y - scale * 0.04,
        hand.x + scale * 0.10,
        hand.y + scale * 0.02,
    );
    ink.fill(&shape_path(&board), HUT_WALL);
    ink.stroke(&shape_path(&board), RIM.with_alpha(0.6), (scale * 0.02).max(0.5));
}

/// A small fish, `size` of full.
fn draw_fish(ink: &mut impl Ink, centre: Point, scale: f64, size: f64) {
    let body = Ellipse::new(centre, (scale * 0.075 * size, scale * 0.042 * size), 0.0);
    ink.fill(&shape_path(&body), FISH);
    let tail = Point::new(centre.x - scale * 0.075 * size, centre.y);
    let mut fin = BezPath::new();
    fin.move_to(tail);
    fin.line_to(Point::new(
        tail.x - scale * 0.045 * size,
        tail.y - scale * 0.035 * size,
    ));
    fin.line_to(Point::new(
        tail.x - scale * 0.045 * size,
        tail.y + scale * 0.035 * size,
    ));
    fin.close_path();
    ink.fill(&fin, FISH);
}

// ---------------------------------------------------------------------------
// The hut
// ---------------------------------------------------------------------------

pub const HUT_WALL: Color = Color::from_rgb8(23, 26, 34);
/// Inside, seen through an open door — darker than the wall, which is what
/// makes it read as a hole rather than a panel.
pub const DOORWAY: Color = Color::from_rgb8(8, 9, 13);
pub const HUT_ROOF: Color = Color::from_rgb8(15, 17, 23);
/// Lamplight. The only warm thing on the frame after dark, which is exactly why
/// it reads from across the room.
pub const LAMP: Color = Color::from_rgb8(255, 186, 92);
pub const LAMP_DEEP: Color = Color::from_rgb8(148, 84, 26);

/// Where the hut and its parts sit, in panel pixels.
pub struct HutGeometry {
    pub left: f64,
    pub base: f64,
    pub width: f64,
    pub height: f64,
}

impl HutGeometry {
    /// `base` is the ground he stands on — the *bottom* of the rail.
    ///
    /// It used to be the rail's top edge, which put the whole hut above the
    /// rail, in the shell's territory. The shell is stacked after the fisherman
    /// and paints straight over it, so the hut was drawn correctly every frame
    /// and covered every frame. Nothing distinguishes that from not drawing it
    /// at all except doing the arithmetic.
    pub fn new(left: f64, base: f64, width: f64, band: f64) -> Self {
        Self {
            left,
            base,
            width,
            // The roof peaks at 1.34 of this, so it has to leave room: a hut
            // that grew out of the top of the rail would vanish the same way.
            height: band * 0.52,
        }
    }

    /// The window, which is the whole point of the hut.
    ///
    /// Placed on the right-hand wall — the side he approaches from — so that
    /// walking home takes him *past* it rather than behind it.
    pub fn window(&self) -> Rect {
        let w = self.width * 0.26;
        let h = self.height * 0.30;
        let x = self.left + self.width * 0.56;
        let y = self.base - self.height * 0.62;
        Rect::new(x, y, x + w, y + h)
    }

    /// The chimney's mouth, where smoke leaves.
    pub fn chimney(&self) -> Point {
        Point::new(
            self.left + self.width * 0.24,
            self.base - self.height * 1.02,
        )
    }
}

/// How brightly the window is lit, 0 to 1.
///
/// **Lit when he is home and awake, and only then.** That is the cue the whole
/// hut exists to give: a warm window means somebody is in, and a dark one means
/// he is out at the water — which you can confirm by looking along the rail.
/// Making it a function of the *routine* rather than of the clock is what keeps
/// the two from ever disagreeing.
///
/// It fades rather than switches, over about a minute of his day, because a
/// lamp being carried into a room is not a light switch.
pub fn window_light(doing: Doing, place: Place, progress: f64) -> f64 {
    let settled = |edge: f64| ease((progress / edge).min(1.0));
    match (doing, place) {
        // Reading by the lamp is the brightest the hut gets.
        (Doing::Reading, _) => settled(0.08),
        // A nap is dim — he does not light the lamp to lie down.
        (Doing::Siesta, _) => 0.22,
        // Going to bed: the lamp goes out over the first minutes.
        (Doing::Sleeping, _) => 0.45 * (1.0 - ease((progress / 0.04).min(1.0))),
        // Waking: a lamp lit before dawn.
        (Doing::Waking, _) => 0.75,
        // Anything else indoors is somebody moving about.
        (_, Place::Hut) => 0.5,
        _ => 0.0,
    }
}

/// Whether the chimney is drawing.
///
/// Whenever there is a reason for a fire inside: he is home and it is dark or
/// cold enough to have lit one. Cheap, and the classic "somebody is home" cue —
/// smoke says the hut is occupied from further away than a window does.
pub fn chimney_smoke(doing: Doing, place: Place) -> f64 {
    match (doing, place) {
        (Doing::Reading, _) | (Doing::Waking, _) => 1.0,
        (Doing::Sleeping, _) => 0.45,
        (Doing::Siesta, _) => 0.25,
        (_, Place::Hut) => 0.7,
        _ => 0.0,
    }
}

/// How many planks the walls are made of.
const PLANKS: usize = 7;

/// How many trips the build takes: one per plank, plus two for the roof and
/// the chimney.
pub const BUILD_TRIPS: f64 = PLANKS as f64 + 2.0;

/// How long the hut takes to go up, in seconds.
pub const BUILD_SECONDS: f64 = 52.0;

/// How much of the hut is standing, 0 to 1.
///
/// **He builds it every time Smithy starts.** Not once per day and not once
/// ever: the launch is the moment somebody is actually looking, and a hut that
/// was simply there when the window opened would be scenery. Watching it go up
/// plank by plank is the difference between a backdrop and somebody's place.
///
/// It is a function of the session's age rather than of the clock, which also
/// means it cannot be half-built at four in the afternoon on a Tuesday.
pub fn hut_completion(seconds_since_launch: f64) -> f64 {
    (seconds_since_launch / BUILD_SECONDS).clamp(0.0, 1.0)
}

/// How far through the build each part appears.
///
/// Walls first and the window last, which is the order it would actually
/// happen in and also the order that reads best — the light coming on at the
/// end is the finish.
pub fn build_stage(completion: f64) -> (usize, bool, bool, bool, bool) {
    let planks = ((completion / 0.58) * PLANKS as f64).floor() as usize;
    (
        planks.min(PLANKS),
        completion >= 0.62, // roof
        completion >= 0.78, // chimney
        completion >= 0.88, // door
        completion >= 0.97, // and the lamp goes on
    )
}

/// Where he is while building, as a fraction of the stage.
///
/// Back and forth between the lumber and the wall, one trip per plank, so the
/// hut goes up at the pace he carries it.
pub fn build_position(completion: f64) -> f64 {
    let trip = (completion * BUILD_TRIPS).fract();
    // Out and back, eased at both ends: he does not turn on a sixpence.
    let there = ease((trip * 2.0).min(1.0));
    let back = ease(((trip - 0.5) * 2.0).max(0.0));
    0.30 - there * 0.24 + back * 0.24
}

#[allow(clippy::too_many_arguments)]
fn draw_hut(
    ink: &mut impl Ink,
    hut: &HutGeometry,
    block: &crate::routine::Block,
    progress: f64,
    frame: u64,
    completion: f64,
    door_open: f64,
) {
    let (left, base, w, h) = (hut.left, hut.base, hut.width, hut.height);
    let (planks, roofed, chimney, doored, lamp) = build_stage(completion);
    let edge = (h * 0.04).max(0.5);
    let lit = window_light(block.doing, block.place, progress);

    ink.begin(Part::Hut);
    // The walls, plank by plank from the ground up.
    for plank in 0..planks {
        let top = base - h * (plank as f64 + 1.0) / PLANKS as f64;
        let bottom = base - h * plank as f64 / PLANKS as f64;
        let board = Rect::new(left, top, left + w, bottom);
        ink.fill(&shape_path(&board), HUT_WALL);
        // Each board keeps its own edge, so the wall reads as planks rather
        // than as a filled rectangle.
        ink.stroke(&shape_path(&board), RIM.with_alpha(0.30), edge * 0.7);
    }

    if roofed {
        // Pitched, and overhanging both walls — the overhang is most of what
        // makes a box read as a building.
        let mut roof = BezPath::new();
        roof.move_to(Point::new(left - w * 0.10, base - h));
        roof.line_to(Point::new(left + w * 0.5, base - h * 1.34));
        roof.line_to(Point::new(left + w * 1.10, base - h));
        roof.close_path();
        ink.fill(&roof, HUT_ROOF);
        ink.stroke(&roof, RIM.with_alpha(0.6), edge);
    }

    if chimney {
        let stack = Rect::new(
            left + w * 0.18,
            base - h * 1.02,
            left + w * 0.30,
            base - h * 0.80,
        );
        ink.fill(&shape_path(&stack), HUT_ROOF);
        ink.stroke(&shape_path(&stack), RIM.with_alpha(0.5), edge * 0.8);
    }

    if doored {
        // The doorway is a hole; the door is a panel over it that narrows as
        // it swings. At this size a panel shrinking to a sliver reads as a door
        // opening far better than any attempt at perspective would.
        let (x0, x1) = (left + w * 0.30, left + w * 0.46);
        let top = base - h * 0.52;
        ink.fill(&shape_path(&Rect::new(x0, top, x1, base)), DOORWAY);
        // Lamplight spills through the open door. The window gets all the
        // attention, but the door opening onto a lit room is the warmer cue —
        // it is what "he just got home" looks like from across the room.
        // Gated on the lamp stage: a door that glows before the lamp is
        // installed is the two cues contradicting each other.
        let spill = if lamp {
            door_glow(lit, door_open)
        } else {
            0.0
        };
        if spill > 0.01 {
            ink.fill(&shape_path(&Rect::new(x0, top, x1, base)), LAMP.with_alpha(spill as f32));
        }
        let panel = x1 - (x1 - x0) * door_open;
        if panel > x0 {
            ink.fill(&shape_path(&Rect::new(x0, top, panel, base)), HUT_ROOF);
            ink.stroke(&shape_path(&Rect::new(x0, top, panel, base)), RIM.with_alpha(0.45), edge * 0.6);
        }
    }

    if !lamp {
        // A chimney cannot draw before there is a hearth under it, and an
        // unglazed hole is not a lit window.
        return;
    }

    draw_window(ink, hut, block, lit, h);
    draw_chimney_smoke(ink, hut, chimney_smoke(block.doing, block.place), frame, h);
}

/// The window, and him behind it.
///
/// He is drawn as a **silhouette against the light**, not lit from the front.
/// At this size that is the only version that reads: a dark shape on a warm
/// rectangle survives being three pixels tall, whereas a figure lit from
/// outside is a grey smudge on a dark one.
fn draw_window(
    ink: &mut impl Ink,
    hut: &HutGeometry,
    block: &crate::routine::Block,
    lit: f64,
    h: f64,
) {
    ink.begin(Part::Hut);
    let pane = hut.window();

    if lit < 0.02 {
        // Dark glass still reads as glass — a hole in the wall would not.
        ink.fill(&shape_path(&pane), Color::from_rgb8(11, 13, 18));
        ink.stroke(&shape_path(&pane), RIM.with_alpha(0.30), (h * 0.02).max(0.4));
        return;
    }

    // The spill first, under the pane: light does not stop at the frame.
    ink.fill(&shape_path(&Rect::new(
            pane.x0 - h * 0.10,
            pane.y0 - h * 0.10,
            pane.x1 + h * 0.10,
            pane.y1 + h * 0.10,
        )), LAMP_DEEP.with_alpha(0.16 * lit as f32));
    ink.fill(&shape_path(&pane), LAMP.with_alpha((0.55 + 0.4 * lit) as f32));

    // Him, inside — a shape on the glass, occupying its lower half so he reads
    // as sitting at a table rather than floating.
    //
    // Not while he is walking home: the block's *destination* is the hut for
    // the whole of that walk, so without the exclusion the window shows him
    // sitting at his table while he is visibly still crossing the rail. Nor
    // while he is getting up: a man at his table is not a man swinging his
    // legs out of bed.
    if block.place == Place::Hut
        && !matches!(block.doing, Doing::Sleeping | Doing::Walking | Doing::Waking)
    {
        let cx0 = pane.x0 + pane.width() * 0.55;
        let head = pane.y0 + pane.height() * 0.34;
        ink.fill(&shape_path(&Circle::new(Point::new(cx0, head), pane.height() * 0.15)), HUT_ROOF);
        // Shoulders, and whatever is in front of him.
        ink.fill(&shape_path(&Rect::new(
                cx0 - pane.width() * 0.22,
                head + pane.height() * 0.12,
                cx0 + pane.width() * 0.20,
                pane.y1,
            )), HUT_ROOF);
        if block.doing == Doing::Reading {
            // The book, edge-on and catching the lamp.
            ink.fill(&shape_path(&Rect::new(
                    cx0 - pane.width() * 0.44,
                    head + pane.height() * 0.22,
                    cx0 - pane.width() * 0.16,
                    head + pane.height() * 0.52,
                )), PAGE.with_alpha(0.85));
        }
    }

    // The mullion, which is what sells it as a window rather than a lamp.
    let mid = pane.x0 + pane.width() * 0.5;
    let mut bar = BezPath::new();
    bar.move_to(Point::new(mid, pane.y0));
    bar.line_to(Point::new(mid, pane.y1));
    ink.stroke(&bar, HUT_ROOF, (h * 0.03).max(0.6));
    ink.stroke(&shape_path(&pane), RIM.with_alpha(0.55), (h * 0.025).max(0.5));
}

fn draw_chimney_smoke(
    ink: &mut impl Ink,
    hut: &HutGeometry,
    strength: f64,
    frame: u64,
    h: f64,
) {
    if strength < 0.05 {
        return;
    }
    ink.begin(Part::Smoke);
    let mouth = hut.chimney();
    for puff in 0..4 {
        let phase = ((frame as f64 / 46.0) + f64::from(puff) * 0.25).fract();
        // Capped so the puff stays on the rail: the shell is stacked after the
        // fisherman and paints over anything higher — drawn and covered is
        // indistinguishable from never drawn. The chimney mouth sits about
        // 0.71 hut-heights below the rail's top.
        let rise = phase * h * 0.65;
        // Drifting as it climbs, and widening — smoke that went straight up in
        // a column would read as a chimney diagram.
        let drift = phase * phase * h * 0.30;
        ink.fill(&shape_path(&Circle::new(
                Point::new(mouth.x + drift, mouth.y - rise),
                (h * 0.05).max(0.7) * (0.6 + phase * 1.9),
            )), SMOKE.with_alpha((1.0 - phase) as f32 * 0.36 * strength as f32));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **Cross-legged, not a lunge**, which is the difference between settling
    /// in and crouching to spring. The distinguishing property is that the shin
    /// folds *back*: the foot returns toward the body rather than continuing
    /// away from it, so it ends up nearer the hip than the knee is.
    ///
    /// The first version had hip 0.64, knee 0.76, foot 0.94 — each further
    /// forward and lower than the last, which is a lunge however it is shaded.
    #[test]
    fn the_seated_pose_folds_its_shin_back_rather_than_lunging() {
        let knee_reach = (SEATED.knee.x - SEATED.hip.x).abs();
        let foot_reach = (SEATED.foot.x - SEATED.hip.x).abs();
        assert!(
            foot_reach < knee_reach,
            "foot reaches {foot_reach:.2} from the hip and the knee only {knee_reach:.2} — \
             the shin is still going forward, which reads as a lunge"
        );
    }

    /// He is sitting on the ground, so his hip is near it. A cross-legged pose
    /// with a high hip is a man hovering.
    ///
    /// Both assertions compare `const`s, which clippy reads as constant-valued —
    /// deliberate, and the same trade `runtime.rs` documents: they guard a
    /// relationship between numbers that are only ever edited by hand, and they
    /// should fail the moment somebody edits one of them in isolation.
    #[test]
    #[allow(clippy::assertions_on_constants)]
    fn the_seated_pose_puts_him_on_the_ground() {
        assert!(
            SEATED.hip.y > 0.70,
            "hip at {:.2} of the box is too high to be sitting on anything",
            SEATED.hip.y
        );
        assert!(
            SEATED.hip.y < SEATED.knee.y,
            "sitting cross-legged the knees are no higher than the hip"
        );
    }

    /// And he is shorter seated than standing, which is what sharing one unit
    /// box is for — the poses are drawn at the same scale.
    #[test]
    fn he_is_shorter_seated_than_standing() {
        let height = |p: &Pose| p.foot.y - (p.head.y - p.head_radius);
        assert!(
            height(&SEATED) < height(&STANDING),
            "seated {:.2} vs standing {:.2}",
            height(&SEATED),
            height(&STANDING)
        );
    }

    /// **The composite must not repeat on any period a viewer could notice.**
    ///
    /// Williams: "CYCLES ARE MECHANICAL AND LOOK JUST LIKE WHAT THEY ARE —
    /// CYCLES", and the illustration is Chuck Jones' three-year-old
    /// granddaughter asking why the same wave keeps lapping on the island. The
    /// fix he gives is to have each part perform on its own timing.
    ///
    /// Round periods phase-lock — 4, 8 and 12 seconds all line up every twelve
    /// — so this checks the actual combined motion rather than trusting that
    /// the constants look unrelated.
    #[test]
    fn the_secondary_motions_never_settle_into_one_visible_loop() {
        let sample = |t: f64| (secondary(t, Doing::Fishing), head_drift(t));
        let start = sample(0.0);

        // Anything under two minutes would be a loop somebody notices while
        // reading a function.
        for tenth in 1..1200 {
            let t = f64::from(tenth) / 10.0;
            let (breath, head) = sample(t);
            let close = (breath - start.0).abs() < 1e-4 && (head - start.1).abs() < 1e-4;
            assert!(
                !close,
                "the whole figure returns to its starting pose after {t:.1}s"
            );
        }
    }

    /// A breath has to move him, and not so much that he bobs like a cork.
    /// Real vertical travel is sub-pixel at this size, so it is exaggerated —
    /// but exaggeration has a ceiling too.
    #[test]
    fn he_breathes_visibly_without_bobbing() {
        let range = |doing| {
            let (mut low, mut high) = (f64::MAX, f64::MIN);
            for tenth in 0..600 {
                let v = secondary(f64::from(tenth) / 10.0, doing);
                low = low.min(v);
                high = high.max(v);
            }
            high - low
        };

        let seated = range(Doing::Fishing);
        // At 36px, 0.03 of figure height is about a pixel — visible, not silly.
        assert!(
            (0.02..0.08).contains(&seated),
            "a seated breath moves him {seated:.4} of his height"
        );
        assert!(
            range(Doing::Walking) > seated,
            "a walk should move him more than sitting still does"
        );
    }

    /// The feet stay on the ground when he breathes. Lifting everything
    /// together is a figure hopping, which is the tell that a bob was bolted on.
    #[test]
    fn breathing_lifts_the_body_and_not_the_feet() {
        let still = SEATED;
        let risen = breathe(still, -0.05);
        assert!(risen.head.y < still.head.y, "his chest should rise");
        assert!(risen.shoulder.y < still.shoulder.y);
        assert_eq!(risen.foot, still.foot, "his feet must stay put");
        assert_eq!(risen.knee, still.knee);
    }

    /// **He has two legs, and they alternate.** One leg reads as a flamingo,
    /// and worse, a walk cycle with a single leg has nothing to alternate — the
    /// stride is invisible and the whole thing looks like sliding.
    #[test]
    fn he_walks_on_two_legs_that_move_independently() {
        let forward = pose_for(Doing::Walking, 0.0, 0.0);
        let mid = pose_for(Doing::Walking, 1.0 / (strides() * 2.0), 0.0);

        // Exactly ten filled parts: two legs of two segments, a trunk, two arm
        // segments, a head, a crown and a brim.
        //
        // `>= 8` was the first version of this and it caught nothing — take a
        // leg away and there are still eight. A threshold that a broken figure
        // satisfies is not a threshold.
        assert_eq!(
            figure_paths(&forward).len(),
            10,
            "the figure has the wrong number of parts — a limb is missing"
        );

        // The two feet are in different places, which is what a stride *is*.
        let near = forward.foot.x;
        let far = forward.hip.x - (forward.foot.x - forward.hip.x) * 0.75;
        assert!(
            (near - far).abs() > 0.05,
            "both feet are at {near:.3} — that is one leg drawn twice"
        );

        // The near foot moves between poses; if both legs were the same shape
        // the silhouette would never change.
        assert!(
            (forward.foot.x - mid.foot.x).abs() > 1e-3,
            "his stride does not move his foot"
        );
    }

    /// A settled activity at progress 0 must not inherit the walk that
    /// delivered him.
    ///
    /// The old facing lookback subtracted a wall-clock second and asked the
    /// routine, so at the start of Cooking (arrived leftward from the perch)
    /// it still saw that walk and kept him facing left. Within-block lookback
    /// clamps progress to zero, both samples sit on the fire, and stillness
    /// wins (+1). The Walking path is rescued by the walk-end beat special
    /// case; this is the case that is not. Once a golden bakes the new
    /// facing in, the old behaviour is not recoverable by inspection.
    #[test]
    fn a_settled_activity_does_not_inherit_the_walk_that_delivered_him() {
        // Perch is to the right of Fire: the walk that put him here was
        // leftward. Under the old lookback he would still be facing left.
        assert!(
            place_position(Place::Perch) > place_position(Place::Fire),
            "the premise needs the delivery walk to be leftward"
        );
        let face = face_for(Place::Perch, Place::Fire, Doing::Cooking, 0.0, 1.0);
        assert_eq!(
            face, 1.0,
            "at progress 0 of Cooking he faced {face}, which means the lookback \
             still reached into the walk that brought him — clamp failed"
        );
    }

    /// **He faces the way he is going.** The poses are drawn facing right, so
    /// anything moving left has to be mirrored — without it he moonwalks, which
    /// is exactly what he did while building, since the lumber is to his right
    /// and the wall to his left and so every loaded trip was made backwards.
    #[test]
    fn he_turns_around_rather_than_walking_backwards() {
        assert_eq!(facing(0.2, 0.6), 1.0, "moving right, facing right");
        assert_eq!(facing(0.6, 0.2), -1.0, "moving left, facing left");
        assert_eq!(facing(0.4, 0.4), 1.0, "standing still keeps his heading");

        // Across a whole build he must turn round at least once per plank —
        // out loaded, back empty.
        let mut turns = 0;
        let mut previous = facing(build_position(0.0), build_position(0.001));
        for step in 1..2000 {
            let a = build_position(f64::from(step - 1) / 2000.0);
            let b = build_position(f64::from(step) / 2000.0);
            let now = facing(a, b);
            if now != previous {
                turns += 1;
            }
            previous = now;
        }
        assert!(
            turns >= PLANKS,
            "{turns} turns over {PLANKS} planks — he is walking one way backwards"
        );
    }

    /// Lookback across a plank-trip boundary used to see the previous inbound,
    /// so the first frames of each outbound were faced the wrong way.
    #[test]
    fn a_plank_trip_does_not_inherit_the_previous_trips_facing() {
        // Just after the first trip boundary: outbound (left), must face left.
        let completion = 1.0 / BUILD_TRIPS + 0.001;
        let face = face_for(Place::Garden, Place::Garden, Doing::Walking, 0.0, completion);
        assert_eq!(
            face, -1.0,
            "at completion {completion:.4} (start of trip 1) he faced {face} — \
             lookback still saw the previous inbound"
        );
    }

    /// At `HANDOVER` the lookback used to clamp into the handover walk at
    /// progress 0, stillness faced right, and the last outbound step was
    /// still landing leftward.
    #[test]
    fn handover_keeps_the_last_outbound_facing_until_he_turns() {
        let face = face_for(Place::Perch, Place::Perch, Doing::Fishing, 0.5, HANDOVER);
        assert_eq!(
            face, -1.0,
            "at HANDOVER he faced {face} — stillness at the seam invented a turn"
        );
    }

    /// **He goes through the door rather than ceasing to exist at the wall.**
    ///
    /// He used to walk to the hut and stop being drawn at the block boundary —
    /// no pause, no door, just gone. Stardew sells the same transition with
    /// four frames of door and a cut, and the part that makes a cut read as a
    /// decision is the beat before it.
    #[test]
    fn he_pauses_at_the_door_and_it_opens_before_he_is_gone() {
        let arriving = |p: f64| {
            (
                position_along(Place::Garden, Place::Hut, Doing::Walking, p),
                door_openness(Doing::Walking, Place::Hut, Place::Garden, p),
            )
        };

        // He is still moving early on, and the door is shut.
        let (early, shut) = arriving(0.4);
        assert_eq!(shut, 0.0, "the door opened while he was still crossing");

        // He has arrived by the time the beat starts, and stays put through it.
        let (at_door, _) = arriving(ARRIVAL);
        let (still_there, _) = arriving(1.0);
        assert!(at_door < early, "he should have moved toward the hut");
        assert!(
            (at_door - still_there).abs() < 1e-9,
            "he drifted during the beat: {at_door:.4} then {still_there:.4}"
        );

        // And the door opens during it.
        assert!(arriving(0.9).1 > 0.0, "the door never opened");
        assert!(
            arriving(1.0).1 > 0.9,
            "the door was still shut when he went in"
        );
    }

    /// Leaving is the reverse: the door is open as he steps out and swings shut
    /// behind him.
    #[test]
    fn the_door_shuts_behind_him_when_he_leaves() {
        let leaving = |p: f64| door_openness(Doing::Walking, Place::Perch, Place::Hut, p);
        assert!(leaving(0.0) > 0.9, "he stepped out through a shut door");
        assert_eq!(
            leaving(0.5),
            0.0,
            "it should be shut by the time he is away"
        );
    }

    /// A step has to land inside the range human walking actually occupies.
    /// This is the test the previous constant would have failed: five strides
    /// over a ninety-second walk is one step every eighteen seconds.
    #[test]
    fn his_stride_is_a_pace_a_person_could_walk_at() {
        // Two footfalls to a gait cycle; it is the footfalls a stopwatch
        // counts, and counting cycles instead is how he once walked at
        // double time while this test passed.
        let per_step = crate::routine::WALK_SECONDS / (strides() * 2.0);
        let per_minute = 60.0 / per_step;
        assert!(
            (80.0..=130.0).contains(&per_minute),
            "{per_minute:.1} steps a minute is outside anything a person does \
             — documented walking is 80 to 120"
        );
    }

    /// Perpendicular distance from `p` to the line through `a` and `b`.
    fn off_line(a: Point, b: Point, p: Point) -> f64 {
        let ab = b - a;
        let ap = p - a;
        (ab.x * ap.y - ab.y * ap.x).abs() / ab.hypot().max(1e-9)
    }

    /// **The stretch must clear the head.** The arm is drawn *before* the head,
    /// so a reach that goes straight up inside the head circle is overpainted
    /// and the stretch reads as a man simply standing there — which is exactly
    /// what the first version did, every morning, at his own doorstep.
    #[test]
    fn the_stretch_reaches_clear_of_the_head() {
        let head_top = STRETCHED.head.y - STRETCHED.head_radius;
        assert!(
            STRETCHED.hand.y < head_top - STRETCHED.head_radius,
            "his hand is at {:.2} and the crown at {head_top:.2} — the reach \
             does not clear his hat",
            STRETCHED.hand.y
        );
        // And the arm is not swallowed by the head on the way up: the elbow
        // has to sit outside the head circle.
        let d = ((STRETCHED.elbow.x - STRETCHED.head.x).powi(2)
            + (STRETCHED.elbow.y - STRETCHED.head.y).powi(2))
        .sqrt();
        assert!(
            d > STRETCHED.head_radius,
            "the elbow is inside the head — the arm is overpainted"
        );
    }

    /// **The shouldered rod rests on the shoulder.** A shaft that crosses the
    /// head reads as a spear; one that floats a hand's width off the shoulder
    /// reads as a diagram. And his hand has to be *on* it, or he is carrying
    /// an invisible something else.
    #[test]
    fn the_shouldered_rod_rests_on_him_not_through_him() {
        let shaft_off_shoulder = off_line(STANDING.rod_butt, STANDING.rod_tip, STANDING.shoulder);
        assert!(
            shaft_off_shoulder < 0.05,
            "the shaft passes {shaft_off_shoulder:.3} off the shoulder — it floats"
        );
        let head_bottom = STANDING.head.y + STANDING.head_radius;
        assert!(
            STANDING.rod_tip.y < head_bottom && STANDING.rod_butt.y > head_bottom,
            "the shaft should dip below his chin only ahead of him, never through the head"
        );
        let shaft_off_hand = off_line(STANDING.rod_butt, STANDING.rod_tip, STANDING.hand);
        assert!(
            shaft_off_hand < 0.05,
            "his hand is {shaft_off_hand:.3} off the shaft — he is gripping air"
        );
    }

    /// **A shut door never glows, and a dark room never spills.** The doorway
    /// light is the product of the two, so the cues cannot contradict.
    #[test]
    fn the_doorway_only_glows_when_there_is_light_to_spill() {
        assert_eq!(door_glow(0.8, 0.0), 0.0, "a shut door spills nothing");
        assert_eq!(door_glow(0.0, 1.0), 0.0, "a dark room spills nothing");
        let ajar = door_glow(0.5, 0.3);
        let open = door_glow(0.5, 1.0);
        assert!(open > ajar, "wider spills more");
        assert!(open <= 0.6, "it is a spill, not a second lamp");
    }
}

#[cfg(test)]
mod hut_tests {
    use super::*;

    /// The hut must sit **inside its rail**, roof and all.
    ///
    /// It did not. `base` was the rail's *top* edge, so the whole building was
    /// drawn above the rail in the shell's territory — and the shell is stacked
    /// after the fisherman, so it painted over it every frame. Drawn correctly,
    /// covered completely, and indistinguishable from never drawing at all
    /// except by doing the arithmetic.
    #[test]
    fn the_hut_stands_on_the_rail_rather_than_above_it() {
        let band = 44.0;
        for window_height in [400.0, 600.0, 1125.0] {
            let hut = HutGeometry::new(60.0, window_height - band * 0.10, 50.0, band);
            let rail_top = window_height - band;

            assert!(
                hut.base <= window_height,
                "its floor is off the bottom of the window"
            );
            let peak = hut.base - hut.height * 1.34;
            assert!(
                peak >= rail_top,
                "the roof reaches {:.1} but the rail starts at {rail_top:.1} — \
                 anything above that is drawn under the shell",
                peak
            );
            assert!(hut.window().y0 > rail_top, "the window is off the rail");
            assert!(hut.chimney().y > rail_top, "the chimney is off the rail");
        }
    }

    /// The hut must also keep clear of the corner ornament: the volutes reach
    /// about `band * 1.6` along the rail — the clearance the vines keep — and
    /// the hut, roof overhang included, must not grow out of the corner stone.
    #[test]
    fn the_hut_clears_the_corner_ornament() {
        let band = 44.0;
        let (scale, stage_left, _) = stage_layout(1280.0, band);
        let hut = HutGeometry::new(stage_left - scale * 0.35, 600.0, scale * 1.45, band);

        let ornament_reach = band * 1.6;
        assert!(
            hut.left >= ornament_reach,
            "the wall starts at {:.1} but the corner ornament reaches {ornament_reach:.1}",
            hut.left
        );
        let roof_left = hut.left - hut.width * 0.10;
        assert!(
            roof_left >= ornament_reach,
            "the roof overhang starts at {roof_left:.1}, inside the ornament's {ornament_reach:.1}"
        );
    }

    /// It goes up in an order that makes sense, and the lamp is last — the
    /// light coming on is the finish.
    #[test]
    fn the_hut_is_built_from_the_ground_up_and_lit_last() {
        let (planks, roof, chimney, door, lamp) = build_stage(0.0);
        assert_eq!(planks, 0, "it starts as bare ground");
        assert!(!roof && !chimney && !door && !lamp);

        // Walls before roof, roof before chimney, lamp last of all.
        let stage_of = |what: fn((usize, bool, bool, bool, bool)) -> bool| {
            (0..=100)
                .map(|p| f64::from(p) / 100.0)
                .find(|c| what(build_stage(*c)))
                .expect("every stage arrives")
        };
        let walls = stage_of(|s| s.0 > 0);
        let roof = stage_of(|s| s.1);
        let chimney = stage_of(|s| s.2);
        let lamp = stage_of(|s| s.4);
        assert!(walls < roof, "he roofed it before he walled it");
        assert!(roof < chimney, "the chimney went up before the roof");
        assert!(chimney < lamp, "the lamp lit before there was a hearth");

        let (planks, roof, chimney, door, lamp) = build_stage(1.0);
        assert_eq!(planks, PLANKS, "every plank is up");
        assert!(roof && chimney && door && lamp, "and it is finished");
    }

    /// He carries a plank *to* the wall and walks back empty-handed, one trip
    /// per plank. Pacing on the spot would read as waiting, not working.
    #[test]
    fn he_makes_one_trip_for_every_plank() {
        let mut trips = 0;
        let mut previous = build_position(0.0);
        let mut rising = true;

        for step in 1..=2000 {
            let at = build_position(f64::from(step) / 2000.0);
            let now_rising = at > previous;
            if rising && !now_rising {
                trips += 1;
            }
            rising = now_rising;
            previous = at;
            assert!(
                (0.0..=0.4).contains(&at),
                "he wandered to {at:.3} of the stage — the lumber is by the hut"
            );
        }

        assert!(
            trips >= PLANKS,
            "{trips} trips for {PLANKS} planks — some arrived on their own"
        );
    }

    /// The build is timed from the launch, so it happens while somebody is
    /// looking, and it finishes rather than running forever.
    #[test]
    fn the_hut_goes_up_once_and_then_stays_up() {
        assert_eq!(hut_completion(0.0), 0.0);
        assert!(hut_completion(BUILD_SECONDS * 0.5) > 0.4);
        assert_eq!(hut_completion(BUILD_SECONDS), 1.0);
        assert_eq!(hut_completion(BUILD_SECONDS * 100.0), 1.0, "it stays built");
    }
}
