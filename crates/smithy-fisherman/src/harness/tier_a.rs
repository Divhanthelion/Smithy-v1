//! Tier A — geometry and containment.
//!
//! Each check exists because of a failure that has already happened here or
//! is one edit away. Thresholds name the failure; widening one to pass is
//! deleting the check.

use crate::fisherman::{
    self as f, door_openness, face_for, position_for, scene_at, stage_layout, window_light, Scene,
    ARRIVAL, BUILD_SECONDS, HANDOVER,
};
use crate::routine::{Doing, Place, WALK_SECONDS};

use super::raster::{self, is_bg, render_scene};
use super::report::{CheckResult, FacingFlip};
use super::{height, launched_built, BAND, DAY, SUNRISE, SUNSET, WIDTH};
use crate::Part;

/// Volute clearance along the rail.
///
/// `stage_layout` documents that the corner ornament reaches about
/// `band * 1.6`; the hut stands near `band * 2.1`. Anything left of 1.6 is
/// the hut growing out of the corner stone — the failure this constant
/// exists to catch. Not 2.1: that would be the hut's own left edge, and
/// the check would be tautological.
pub const VOLUTE_CLEARANCE: f64 = 1.6;

/// Max stage-units he may travel in one simulated second on a routine day.
///
/// Longest walk is Hut→Perch ≈ 1.0 stage unit, covered in `ARRIVAL *
/// WALK_SECONDS` under smoothstep whose max derivative is 1.5× linear.
/// Without this bound every discontinuity in the routine passes silently —
/// exactly what `ARRIVAL` exists to prevent. If this fires, investigate
/// before widening.
pub const MAX_DELTA_PER_SECOND: f64 = 1.5 * (1.0 / (ARRIVAL * WALK_SECONDS));

/// Max stage-units/second during the build + handover.
///
/// The handover compresses a walk of up to ~1.0 stage units into
/// `(1 − HANDOVER) * BUILD_SECONDS` of session time (then ARRIVAL). That is
/// faster than a routine walk by design of the session clock — a continuous
/// handover peaks near this value (measured 0.261 on a Perch handover,
/// 2026-08-03). A true teleport jumps beyond it in one sample. Not a
/// widening of [`MAX_DELTA_PER_SECOND`]; a separate bound for a separate
/// clock.
pub const MAX_BUILD_DELTA_PER_SECOND: f64 =
    1.5 * (1.0 / ((1.0 - HANDOVER) * BUILD_SECONDS * ARRIVAL));

/// Epsilon below which a leftward Δ is ignored (float noise, not a step).
const MOONWALK_EPS: f64 = 1e-6;

/// Max |Δ| per second for `window_light` / `door_open`.
///
/// Door openness is the fastest intentional cue: it eases open over the
/// arrival beat (`1 − ARRIVAL` of a walk) under smoothstep, whose max
/// derivative is 1.5× linear. Same derivation as [`MAX_DELTA_PER_SECOND`].
/// The midnight lamp flare jumped 0.450 in one second — well above this.
pub const MAX_LIGHT_DELTA_PER_SECOND: f64 = 1.5 / ((1.0 - ARRIVAL) * WALK_SECONDS);

pub fn run_all() -> Vec<CheckResult> {
    vec![
        he_stays_on_the_rail(),
        he_exists(),
        hidden_indoors(),
        right_size(),
        does_not_teleport(),
        does_not_moonwalk(),
        lighting_continuity(),
        facing_continuity(),
    ]
}

fn sample_scene(hours: f64, launched: f64, frame: u64) -> Scene {
    scene_at(
        WIDTH,
        height(),
        BAND,
        hours,
        SUNRISE,
        SUNSET,
        DAY,
        launched,
        frame,
    )
}

/// Representative outdoor moments across the day (built hut).
fn outdoor_samples() -> Vec<Scene> {
    // Mid-block hours chosen so smoking overlays don't dominate; one per
    // outdoor activity plus a walk. The "snapped out of existence" bug was
    // only tested at the door — the other four places were untested.
    let hours = [
        7.0,  // exercising / coffee-ish
        8.0,  // gardening
        10.0, // fishing
        12.0, // cooking / eating
        16.0, // fishing again
        18.5, // cooking dinner
        19.3, // walking home-ish
    ];
    hours
        .iter()
        .map(|&h| sample_scene(h, launched_built(), (h * 5.0 * 3600.0) as u64))
        .filter(|s| !s.doing.is_indoors(s.place))
        .collect()
}

fn he_stays_on_the_rail() -> CheckResult {
    // Sample outdoor + build frames; count non-bg pixels that invade the
    // volute OR figure-tagged pixels that leave the rail band vertically
    // (rod into the editor pane). Hut roof/smoke are allowed above the
    // band — they are Part::Hut / Part::Smoke, not Figure.
    let mut offenders = 0u64;
    let mut scenes = outdoor_samples();
    // Mid-build: walls rising near the corner stone is the original case.
    scenes.push(sample_scene(10.0, BUILD_SECONDS * 0.55, 40));
    scenes.push(sample_scene(10.0, BUILD_SECONDS * 0.85, 40));

    let rail_top = height() - BAND;
    let x_min = BAND * VOLUTE_CLEARANCE;

    for scene in &scenes {
        let ink = render_scene(scene);
        for y in 0..ink.height() {
            for x in 0..ink.width() {
                let Some((r, g, b, _)) = ink.pixel(x, y) else {
                    continue;
                };
                let rgb = (r, g, b);
                if is_bg(rgb) {
                    continue;
                }
                let xf = x as f64;
                let yf = y as f64;
                if xf < x_min {
                    offenders += 1;
                    continue;
                }
                // Figure body on the rail only — hut/smoke above the band are fine.
                if ink.part_at(x, y) == Some(Part::Figure)
                    && (yf < rail_top - 1.0 || yf >= height())
                {
                    offenders += 1;
                }
            }
        }
    }

    CheckResult {
        name: "he_stays_on_the_rail",
        tier: "A",
        pass: offenders == 0,
        measured: offenders as f64,
        threshold: Some(0.0),
        detail: format!(
            "{offenders} pixels outside volute clearance (band*{VOLUTE_CLEARANCE}) or figure above rail"
        ),
        flips: vec![],
    }
}

fn he_exists() -> CheckResult {
    // Outdoors → figure Part bbox non-empty and inside the stage. Guards the
    // "snapped straight out of existence at the wall" class at every place,
    // not only the door. Tagged, not coloured: IRON cannot tell figure from
    // hut AA.
    let (_, stage_left, stage) = stage_layout(WIDTH, BAND);
    let mut failures = 0u64;
    let mut checked = 0u64;

    for scene in outdoor_samples() {
        if scene.doing.is_indoors(scene.place) {
            continue;
        }
        checked += 1;
        let ink = render_scene(&scene);
        let Some((min_x, min_y, max_x, max_y)) = ink.part_bounds(Part::Figure) else {
            failures += 1;
            continue;
        };
        let inside = min_x as f64 >= stage_left - BAND
            && max_x as f64 <= stage_left + stage + BAND * 3.0
            && min_y as f64 >= height() - BAND - 2.0
            && max_y as f64 <= height();
        if !inside {
            failures += 1;
        }
    }

    CheckResult {
        name: "he_exists",
        tier: "A",
        pass: failures == 0 && checked > 0,
        measured: failures as f64,
        threshold: Some(0.0),
        detail: format!(
            "{failures} outdoor frames with empty or out-of-stage figure bbox ({checked} checked)"
        ),
        flips: vec![],
    }
}

fn hidden_indoors() -> CheckResult {
    // Indoors → zero Part::Figure on the rail. The window silhouette is
    // tagged Hut (it is drawn into the pane as hut décor), so it does not
    // trip this. Sampled across every 15 min of the simulated day — the
    // colour classifier used to report 1–2 false "figure" pixels on half
    // the indoor frames; the part mask makes "0 across all 96" meaningful.
    // Also: when window_light says lit, warm/lamp pixels near the window;
    // Sleeping mid-block may fade the lamp and that is correct.
    let mut figure_on_rail = 0u64;
    let mut light_failures = 0u64;
    let mut checked = 0u64;
    let mut indoor_tiles = 0u64;

    for i in 0..96u32 {
        let hours = (i * 15) as f64 / 60.0;
        let scene = sample_scene(hours, launched_built(), (hours * 18000.0) as u64);
        if !scene.doing.is_indoors(scene.place) {
            continue;
        }
        indoor_tiles += 1;
        checked += 1;
        let lit = window_light(scene.doing, scene.place, scene.progress);
        let ink = render_scene(&scene);
        let rail_top = (height() - BAND) as u32;

        for y in rail_top..ink.height() {
            for x in 0..ink.width() {
                if ink.part_at(x, y) == Some(Part::Figure) {
                    figure_on_rail += 1;
                }
            }
        }

        if lit > 0.05 {
            let (scale, stage_left, _) = stage_layout(WIDTH, BAND);
            let hut = f::HutGeometry::new(
                stage_left - scale * 0.35,
                height() - BAND * 0.10,
                scale * 1.45,
                BAND,
            );
            let win = hut.window();
            let mut warm = 0u64;
            let x0 = win.x0.max(0.0) as u32;
            let x1 = win.x1.min(WIDTH) as u32;
            let y0 = win.y0.max(0.0) as u32;
            let y1 = win.y1.min(height()) as u32;
            for y in y0..y1 {
                for x in x0..x1 {
                    let Some((r, g, b, _)) = ink.pixel(x, y) else {
                        continue;
                    };
                    if raster::is_lamp_warm((r, g, b)) || (r > 120 && g > 60 && b < 100) {
                        warm += 1;
                    }
                }
            }
            if warm == 0 {
                light_failures += 1;
            }
        }
    }

    let pass = figure_on_rail == 0 && light_failures == 0 && checked > 0;
    CheckResult {
        name: "hidden_indoors",
        tier: "A",
        pass,
        measured: (figure_on_rail + light_failures) as f64,
        threshold: Some(0.0),
        detail: format!(
            "{figure_on_rail} figure-tagged pixels on rail while indoors; \
             {light_failures} lit-expected frames with no warm window pixels \
             ({checked} indoor tiles of 96, {indoor_tiles} indoor)"
        ),
        flips: vec![],
    }
}

fn right_size() -> CheckResult {
    // Figure bbox height ∈ [0.5, 1.1] × scale — catches a smudge or a giant
    // from a scale bug. Cheap; whole class. Part mask, not IRON colour.
    let (scale, _, _) = stage_layout(WIDTH, BAND);
    let lo = 0.5 * scale;
    let hi = 1.1 * scale;
    let mut worst = 0.0;
    let mut failures = 0u64;

    for scene in outdoor_samples() {
        let ink = render_scene(&scene);
        let Some((_, min_y, _, max_y)) = ink.part_bounds(Part::Figure) else {
            failures += 1;
            continue;
        };
        let h = (max_y - min_y + 1) as f64;
        worst = if failures == 0 && worst == 0.0 {
            h
        } else if h < lo || h > hi {
            h
        } else {
            worst.max(h)
        };
        if h < lo || h > hi {
            failures += 1;
            worst = h;
        }
    }

    CheckResult {
        name: "right_size",
        tier: "A",
        pass: failures == 0,
        measured: worst,
        threshold: Some(hi),
        detail: format!(
            "{failures} frames with figure bbox height outside [{lo:.1}, {hi:.1}] (scale={scale:.1})"
        ),
        flips: vec![],
    }
}

fn does_not_teleport() -> CheckResult {
    // Routine day at 1 s steps against the walk budget; build/handover
    // against its own compressed-clock budget. Highest-value check here.
    let mut max_day = 0.0;
    let mut at_day = 0u64;
    let mut prev = position_of(&sample_scene(0.0, launched_built(), 0));
    for sec in 1..86_400u64 {
        let hours = sec as f64 / 3600.0;
        let scene = sample_scene(hours, launched_built(), sec);
        let along = position_of(&scene);
        let d = (along - prev).abs();
        if d > max_day {
            max_day = d;
            at_day = sec;
        }
        prev = along;
    }

    let steps = 2_000u64;
    let mut max_build = 0.0;
    let mut at_build = 0u64;
    let mut prev_c = position_of(&sample_scene(10.0, 0.0, 0));
    let dt = BUILD_SECONDS / steps as f64;
    for i in 1..=steps {
        let launched = BUILD_SECONDS * (i as f64 / steps as f64);
        let scene = sample_scene(10.0, launched, i);
        let along = position_of(&scene);
        let d = (along - prev_c).abs() / dt.max(1e-9);
        if d > max_build {
            max_build = d;
            at_build = i;
        }
        prev_c = along;
    }

    let day_ok = max_day < MAX_DELTA_PER_SECOND;
    let build_ok = max_build < MAX_BUILD_DELTA_PER_SECOND;
    // Report utilisation of each budget (1.0 = at the limit). Measured is the
    // worse of the two so a single number cannot look green while one clock
    // is over — the previous report put build's absolute Δ next to the day's
    // limit and read as a contradiction.
    let day_util = max_day / MAX_DELTA_PER_SECOND;
    let build_util = max_build / MAX_BUILD_DELTA_PER_SECOND;
    let measured = day_util.max(build_util);
    CheckResult {
        name: "does_not_teleport",
        tier: "A",
        pass: day_ok && build_ok,
        measured,
        threshold: Some(1.0),
        detail: format!(
            "day max |Δ|/s={max_day:.6} at {at_day} (lim {MAX_DELTA_PER_SECOND:.6}, util {day_util:.3}); \
             build max |Δ|/s={max_build:.6} at {at_build} (lim {MAX_BUILD_DELTA_PER_SECOND:.6}, util {build_util:.3})"
        ),
        flips: vec![],
    }
}

fn does_not_moonwalk() -> CheckResult {
    // Whenever Δposition < −ε, facing must be < 0. Trip-boundary lookback
    // and the HANDOVER stillness case both lived here — 29 frames, left
    // red on purpose until face_for clamped trips and looked into the
    // last outbound at handover.
    let mut violations = 0u64;
    let mut prev_along = position_of(&sample_scene(0.0, launched_built(), 0));

    for sec in 1..86_400u64 {
        let hours = sec as f64 / 3600.0;
        let scene = sample_scene(hours, launched_built(), sec);
        let along = position_of(&scene);
        let face = face_of(&scene);
        let delta = along - prev_along;
        if delta < -MOONWALK_EPS && face >= 0.0 {
            violations += 1;
        }
        prev_along = along;
    }

    let steps = 2_000u64;
    let mut build_completions: Vec<f64> = Vec::new();
    let mut prev_along = position_of(&sample_scene(10.0, 0.0, 0));
    for i in 1..=steps {
        let launched = BUILD_SECONDS * (i as f64 / steps as f64);
        let completion = launched / BUILD_SECONDS;
        let scene = sample_scene(10.0, launched, i);
        let along = position_of(&scene);
        let face = face_of(&scene);
        let delta = along - prev_along;
        if delta < -MOONWALK_EPS && face >= 0.0 {
            violations += 1;
            build_completions.push(completion);
        }
        prev_along = along;
    }

    let mut boundaries: Vec<f64> = Vec::new();
    for &c in &build_completions {
        if boundaries
            .last()
            .map(|&prev| (c - prev).abs() > 0.001)
            .unwrap_or(true)
        {
            boundaries.push(c);
        }
    }
    let boundary_list = boundaries
        .iter()
        .map(|c| format!("{c:.4}"))
        .collect::<Vec<_>>()
        .join(", ");

    CheckResult {
        name: "does_not_moonwalk",
        tier: "A",
        pass: violations == 0,
        measured: violations as f64,
        threshold: Some(0.0),
        detail: format!(
            "{violations} steps with Δposition < 0 while facing ≥ 0; \
             build completions: [{boundary_list}]"
        ),
        flips: vec![],
    }
}

fn lighting_continuity() -> CheckResult {
    // Same shape as does_not_teleport, for window_light and door_open —
    // but only *within* a (doing, place) stretch. Door snaps open at the
    // first frame of a leaving walk and shut when an arriving walk ends;
    // the waking lamp steps from 0 to 0.75. Those are block seams, not
    // discontinuities. The midnight lamp flare was Sleeping/Hut on both
    // sides with progress resetting — same doing+place, so it still trips.
    //
    // Sample across midnight with the day seed rolling — a single DAY-0
    // sweep never compares 23:59 to 00:00.
    let mut max_lamp = 0.0;
    let mut max_door = 0.0;
    let mut at_lamp = 0u64;
    let mut at_door = 0u64;

    let mut prev_scene = sample_at_abs(0);
    let mut prev = light_of(&prev_scene);
    // 36 h: one full day plus the morning after, so the seam is inside.
    for sec in 1..(86_400u64 + 12 * 3600) {
        let scene = sample_at_abs(sec);
        let now = light_of(&scene);
        let same_stretch = scene.doing == prev_scene.doing && scene.place == prev_scene.place;
        if same_stretch {
            let d_lamp = (now.0 - prev.0).abs();
            let d_door = (now.1 - prev.1).abs();
            if d_lamp > max_lamp {
                max_lamp = d_lamp;
                at_lamp = sec;
            }
            if d_door > max_door {
                max_door = d_door;
                at_door = sec;
            }
        }
        prev = now;
        prev_scene = scene;
    }

    let lamp_ok = max_lamp < MAX_LIGHT_DELTA_PER_SECOND;
    let door_ok = max_door < MAX_LIGHT_DELTA_PER_SECOND;
    let util = (max_lamp / MAX_LIGHT_DELTA_PER_SECOND).max(max_door / MAX_LIGHT_DELTA_PER_SECOND);
    CheckResult {
        name: "lighting_continuity",
        tier: "A",
        pass: lamp_ok && door_ok,
        measured: util,
        threshold: Some(1.0),
        detail: format!(
            "within-stretch lamp max |Δ|/s={max_lamp:.6} at {at_lamp}s; \
             door max |Δ|/s={max_door:.6} at {at_door}s \
             (lim {MAX_LIGHT_DELTA_PER_SECOND:.6}; block seams excluded)"
        ),
        flips: vec![],
    }
}

/// Scene at an absolute second from local midnight of [`DAY`], day seed rolling.
fn sample_at_abs(sec: u64) -> Scene {
    let hours_abs = sec as f64 / 3600.0;
    let day = DAY + (hours_abs / 24.0).floor() as i64;
    let hours = hours_abs.rem_euclid(24.0);
    scene_at(
        WIDTH,
        height(),
        BAND,
        hours,
        SUNRISE,
        SUNSET,
        day,
        launched_built(),
        sec,
    )
}

fn light_of(scene: &Scene) -> (f64, f64) {
    let lamp = window_light(scene.doing, scene.place, scene.progress);
    let door = if scene.completion < 1.0 {
        let handover = ((scene.completion - HANDOVER) / (1.0 - HANDOVER)).clamp(0.0, 1.0);
        door_openness(Doing::Walking, scene.place, Place::Garden, handover)
    } else {
        door_openness(scene.doing, scene.place, scene.previous, scene.progress)
    };
    (lamp, door)
}

fn facing_continuity() -> CheckResult {
    // Report-only: every frame where face flips. Makes the lookback
    // judgement call inspectable. Never fails the process for flip count.
    let mut flips = Vec::new();
    let mut prev_face = face_of(&sample_scene(0.0, launched_built(), 0));

    for sec in 1..86_400u64 {
        let hours = sec as f64 / 3600.0;
        let scene = sample_scene(hours, launched_built(), sec);
        let face = face_of(&scene);
        if (face - prev_face).abs() > 0.5 {
            flips.push(FacingFlip {
                second: sec,
                hours,
                from: prev_face,
                to: face,
                doing: format!("{:?}", scene.doing),
                place: format!("{:?}", scene.place),
            });
        }
        prev_face = face;
    }

    let n = flips.len();
    CheckResult {
        name: "facing_continuity",
        tier: "A",
        pass: true, // report-only
        measured: n as f64,
        threshold: None,
        detail: "report only — list of facing flips across the simulated day".into(),
        flips,
    }
}

fn position_of(scene: &Scene) -> f64 {
    position_for(
        scene.previous,
        scene.place,
        scene.doing,
        scene.progress,
        scene.completion,
    )
    .1
}

fn face_of(scene: &Scene) -> f64 {
    face_for(
        scene.previous,
        scene.place,
        scene.doing,
        scene.progress,
        scene.completion,
    )
}
