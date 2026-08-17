//! Tier B — the picture.
//!
//! Thresholds are set from a measured known-good frame and documented with
//! that measurement. Widening a threshold to pass is deleting the check; if
//! a check is red for a real bug, leave it red.

use kurbo::Point;

use crate::fisherman::{
    self as f, door_glow, door_openness, place_position, scene_at, stage_layout, window_light,
    Scene, BUILD_SECONDS,
};
use crate::routine::{Doing, Place};

use super::raster::{is_bg, is_lamp_warm, is_rim, luminance, render_scene, STEEL_BODY};
use super::report::CheckResult;
use super::{height, launched_built, BAND, DAY, SUNRISE, SUNSET, WIDTH};

pub fn run_all() -> Vec<CheckResult> {
    vec![
        contrast(),
        ink_budget(),
        fire_where_fire_is(),
        light_agrees(),
    ]
}

fn sample(hours: f64, launched: f64, frame: u64) -> Scene {
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

fn contrast() -> CheckResult {
    // Mean luminance of RIM-ish stroke pixels vs STEEL_BODY behind them.
    // Without this he vanishes against the frame — previously only judged
    // by eye, and only in the light the tester happened to render.
    //
    // Measured 2026-08-03 on fishing@10h (built): mean RIM luminance ≈ 148,
    // STEEL_BODY luminance ≈ 47, delta ≈ 101. Threshold 40 leaves headroom
    // for AA without accepting a near-invisible rim.
    const MIN_DELTA: f64 = 40.0;

    let scene = sample(10.0, launched_built(), 40);
    let ink = render_scene(&scene);
    let steel = {
        let c = STEEL_BODY.to_rgba8();
        luminance((c.r, c.g, c.b))
    };
    let mut sum = 0.0;
    let mut n = 0u64;
    for y in 0..ink.height() {
        for x in 0..ink.width() {
            let Some((r, g, b, _)) = ink.pixel(x, y) else {
                continue;
            };
            if is_rim((r, g, b)) {
                sum += luminance((r, g, b));
                n += 1;
            }
        }
    }
    let mean = if n == 0 { 0.0 } else { sum / n as f64 };
    let delta = mean - steel;

    CheckResult {
        name: "contrast",
        tier: "B",
        pass: n > 0 && delta > MIN_DELTA,
        measured: delta,
        threshold: Some(MIN_DELTA),
        detail: format!(
            "mean RIM luminance {mean:.1} − steel {steel:.1} = {delta:.1} over {n} pixels; min {MIN_DELTA}"
        ),
        flips: vec![],
    }
}

fn ink_budget() -> CheckResult {
    // Non-background coverage per representative tile. Blank frame or solid
    // blob both fail. Measured 2026-08-03 on 1100×132 tiles:
    //   fishing 0.0122, cooking 0.0115, reading 0.0094, build-55% 0.0077.
    // LO 0.005 sits under the sparsest known-good (mid-build); HI 0.25 is
    // far above a full scene — a solid blob would be ~1.0.
    const LO: f64 = 0.005;
    const HI: f64 = 0.25;

    let scenes = [
        sample(10.0, launched_built(), 40),
        sample(10.0, BUILD_SECONDS * 0.55, 40),
        sample(20.5, launched_built(), 60), // reading indoors
        sample(18.5, launched_built(), 40), // cooking
    ];
    let mut worst = 0.0;
    let mut failures = 0u64;

    for scene in &scenes {
        let ink = render_scene(scene);
        let total = (ink.width() * ink.height()) as f64;
        let mut painted = 0u64;
        for y in 0..ink.height() {
            for x in 0..ink.width() {
                let Some((r, g, b, _)) = ink.pixel(x, y) else {
                    continue;
                };
                if !is_bg((r, g, b)) {
                    painted += 1;
                }
            }
        }
        let frac = painted as f64 / total;
        if frac < LO || frac > HI {
            failures += 1;
            worst = frac;
        } else if worst == 0.0 {
            worst = frac;
        }
    }

    CheckResult {
        name: "ink_budget",
        tier: "B",
        pass: failures == 0,
        measured: worst,
        threshold: Some(HI),
        detail: format!("{failures} tiles with non-bg coverage outside [{LO}, {HI}]"),
        flips: vec![],
    }
}

fn fire_where_fire_is() -> CheckResult {
    // Part::Fire pixels must sit near the pit. "A hearth that teleports to
    // the doorstep reads as a decal" — the comment at paint's fire_base is
    // the assertion. Tagged, not coloured: the part mask is exact.
    let scene = sample(18.5, launched_built(), 40); // cooking at the fire
                                                    // Force Cooking/Fire if the clock landed elsewhere (cigarette overlay).
    let scene = Scene {
        doing: Doing::Cooking,
        place: Place::Fire,
        previous: Place::Perch,
        progress: 0.5,
        completion: 1.0,
        frame: 40,
        seconds: 8.0,
        ..scene
    };
    let ink = render_scene(&scene);
    let (scale, stage_left, stage) = stage_layout(WIDTH, BAND);
    let top = height() - BAND + (BAND - scale) * 0.55;
    let fire_base = Point::new(
        stage_left + place_position(Place::Fire) * stage + scale * 0.80,
        top + scale * 0.92,
    );
    // Generous pit bbox — flames flicker and sparks rise; the failure mode
    // is the whole hearth at the doorstep, not a spark one band high.
    let pad = scale * 0.55;
    let x0 = (fire_base.x - pad).max(0.0);
    let x1 = (fire_base.x + pad).min(WIDTH);
    let y0 = (fire_base.y - pad * 1.2).max(0.0);
    let y1 = (fire_base.y + pad * 0.4).min(height());

    let mut fire_total = 0u64;
    let mut fire_in = 0u64;
    for y in 0..ink.height() {
        for x in 0..ink.width() {
            if ink.part_at(x, y) != Some(crate::Part::Fire) {
                continue;
            }
            fire_total += 1;
            let xf = x as f64;
            let yf = y as f64;
            if xf >= x0 && xf <= x1 && yf >= y0 && yf <= y1 {
                fire_in += 1;
            }
        }
    }

    // At least some fire, and ≥ 85% of it inside the pit bbox. Measured
    // 2026-08-03 cooking frame: all FIRE_* cores landed in-bbox; 0.85 leaves
    // AA fringe without accepting a doorstep hearth.
    const MIN_IN_FRAC: f64 = 0.85;
    let frac = if fire_total == 0 {
        0.0
    } else {
        fire_in as f64 / fire_total as f64
    };
    let pass = fire_total > 0 && frac >= MIN_IN_FRAC;

    CheckResult {
        name: "fire_where_fire_is",
        tier: "B",
        pass,
        measured: frac,
        threshold: Some(MIN_IN_FRAC),
        detail: format!(
            "{fire_in}/{fire_total} Fire-tagged pixels inside pit bbox (frac {frac:.3})"
        ),
        flips: vec![],
    }
}

fn light_agrees() -> CheckResult {
    // door_glow > 0 ⇒ warm lit pixels near the doorway; lamp out ⇒ none.
    // "A shut door never glows and a dark room never spills."
    let mut failures = 0u64;
    let measured;

    // Arriving walk with door opening — glow should spill.
    let arriving = Scene {
        width: WIDTH,
        height: height(),
        band: BAND,
        doing: Doing::Walking,
        place: Place::Hut,
        previous: Place::Garden,
        progress: 0.92,
        completion: 1.0,
        frame: 40,
        seconds: 8.0,
    };
    let lit = window_light(Doing::Reading, Place::Hut, 0.5); // bright lamp inside
                                                             // door_glow uses the scene's own lit via draw_hut's window_light(block).
                                                             // For Walking/Hut, window_light is 0.5 (anything else indoors branch is
                                                             // place-based only when doing isn't special-cased — Walking at Hut is
                                                             // NOT indoors for is_indoors, and window_light(_, Hut) for non-special
                                                             // doing hits `(_, Place::Hut) => 0.5`).
    let door = door_openness(
        arriving.doing,
        arriving.place,
        arriving.previous,
        arriving.progress,
    );
    let glow = door_glow(
        window_light(arriving.doing, arriving.place, arriving.progress),
        door,
    );
    let ink = render_scene(&arriving);
    let (scale, stage_left, _) = stage_layout(WIDTH, BAND);
    let hut = f::HutGeometry::new(
        stage_left - scale * 0.35,
        height() - BAND * 0.10,
        scale * 1.45,
        BAND,
    );
    // Doorway region: left portion of the hut wall.
    let door_x0 = hut.left;
    let door_x1 = hut.left + hut.width * 0.45;
    let door_y0 = hut.base - hut.height;
    let door_y1 = hut.base;
    let mut warm = 0u64;
    for y in door_y0.max(0.0) as u32..door_y1.min(height()) as u32 {
        for x in door_x0.max(0.0) as u32..door_x1.min(WIDTH) as u32 {
            let Some((r, g, b, _)) = ink.pixel(x, y) else {
                continue;
            };
            if is_lamp_warm((r, g, b)) || (r > 100 && g > 50 && b < 90) {
                warm += 1;
            }
        }
    }
    measured = glow;
    if glow > 0.05 && warm == 0 {
        failures += 1;
    }

    // Sleeping mid-block, door shut: no doorway spill.
    let asleep = sample(23.5, launched_built(), 60);
    let asleep = Scene {
        doing: Doing::Sleeping,
        place: Place::Hut,
        previous: Place::Doorstep,
        progress: 0.5,
        completion: 1.0,
        frame: 60,
        seconds: 12.0,
        ..asleep
    };
    let door2 = door_openness(asleep.doing, asleep.place, asleep.previous, asleep.progress);
    let glow2 = door_glow(
        window_light(asleep.doing, asleep.place, asleep.progress),
        door2,
    );
    let ink2 = render_scene(&asleep);
    let mut warm2 = 0u64;
    for y in door_y0.max(0.0) as u32..door_y1.min(height()) as u32 {
        for x in door_x0.max(0.0) as u32..door_x1.min(WIDTH) as u32 {
            let Some((r, g, b, _)) = ink2.pixel(x, y) else {
                continue;
            };
            // Doorway fill is DOORWAY (near-black) when shut; LAMP spill
            // would be the bug.
            if is_lamp_warm((r, g, b)) {
                warm2 += 1;
            }
        }
    }
    if glow2 <= 0.01 && warm2 > 20 {
        // A few AA fringe pixels from the window are tolerable; a glowing
        // shut door is not. Threshold 20 measured as window bleed upper
        // bound on a dark sleeping frame (2026-08-03).
        failures += 1;
    }

    let _ = lit; // cited above for the comment trail
    CheckResult {
        name: "light_agrees",
        tier: "B",
        pass: failures == 0,
        measured,
        threshold: Some(0.0),
        detail: format!(
            "{failures} disagreements (arriving glow={glow:.3} warm={warm}; sleeping glow={glow2:.3} warm={warm2})"
        ),
        flips: vec![],
    }
}
