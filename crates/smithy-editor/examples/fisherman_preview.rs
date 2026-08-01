//! Headless renderer for the fisherman: draws him to PNGs so his silhouette
//! can be *looked at* rather than tuned blind.
//!
//!     cargo run -p smithy-editor --example fisherman_preview
//!
//! Writes `/tmp/fisherman/*.png`:
//!   poses.png   — every activity, large, in a labelled grid order (see stdout)
//!   walk.png    — a full stride, frame by frame
//!   scene_1x.png / scene_3x.png — the whole rail at true size and zoomed:
//!                 build stages, door beats, fire, window light, the works
//!
//! The drawing mirrors `fisherman.rs`'s own math — same constants, same
//! geometry functions — so what lands in the PNG is what lands on the rail.

use floem::peniko::kurbo::{BezPath, Circle, Ellipse, PathEl, Point, Rect, Shape};
use floem::peniko::Color;

use smithy_editor::fisherman as f;
use smithy_editor::routine::{Doing, Place};

// The palette, copied from fisherman.rs (the constants there are private).
const IRON: Color = Color::from_rgb8(17, 20, 27);
const RIM: Color = Color::from_rgb8(186, 148, 72);
const LINE: Color = Color::from_rgb8(126, 138, 162);
const FIRE_CORE: Color = Color::from_rgb8(255, 224, 150);
const FIRE_BODY: Color = Color::from_rgb8(226, 132, 44);
const FIRE_DEEP: Color = Color::from_rgb8(126, 48, 18);
const FISH: Color = Color::from_rgb8(150, 172, 196);
const SMOKE: Color = Color::from_rgb8(150, 158, 172);
const GREEN: Color = Color::from_rgb8(96, 138, 84);
const PAGE: Color = Color::from_rgb8(196, 190, 172);
const HUT_WALL: Color = Color::from_rgb8(23, 26, 34);
const DOORWAY: Color = Color::from_rgb8(8, 9, 13);
const HUT_ROOF: Color = Color::from_rgb8(15, 17, 23);
const LAMP: Color = Color::from_rgb8(255, 186, 92);
const LAMP_DEEP: Color = Color::from_rgb8(148, 84, 26);
// The frame's steel, from forged.rs, as a backdrop to judge contrast against.
const STEEL_DEEP: Color = Color::from_rgb8(20, 24, 34);
const STEEL_BODY: Color = Color::from_rgb8(41, 49, 66);

const PLANKS: usize = 7;

// ---------------------------------------------------------------------------
// Rasterising kurbo paths with tiny-skia
// ---------------------------------------------------------------------------

fn tc(c: Color) -> tiny_skia::Color {
    let c = c.to_rgba8();
    tiny_skia::Color::from_rgba8(c.r, c.g, c.b, c.a)
}

fn to_ts(path: &BezPath, at: &impl Fn(Point) -> Point) -> Option<tiny_skia::Path> {
    let mut pb = tiny_skia::PathBuilder::new();
    for el in path.elements() {
        match *el {
            PathEl::MoveTo(p) => {
                let p = at(p);
                pb.move_to(p.x as f32, p.y as f32);
            }
            PathEl::LineTo(p) => {
                let p = at(p);
                pb.line_to(p.x as f32, p.y as f32);
            }
            PathEl::QuadTo(a, b) => {
                let a = at(a);
                let b = at(b);
                pb.quad_to(a.x as f32, a.y as f32, b.x as f32, b.y as f32);
            }
            PathEl::CurveTo(a, b, c) => {
                let a = at(a);
                let b = at(b);
                let c = at(c);
                pb.cubic_to(a.x as f32, a.y as f32, b.x as f32, b.y as f32, c.x as f32, c.y as f32);
            }
            PathEl::ClosePath => pb.close(),
        }
    }
    pb.finish()
}

struct Sheet {
    pm: tiny_skia::Pixmap,
}

impl Sheet {
    fn new(w: u32, h: u32, bg: Color) -> Self {
        let mut pm = tiny_skia::Pixmap::new(w, h).expect("pixmap");
        pm.fill(tc(bg));
        Sheet { pm }
    }

    fn paint(color: Color) -> tiny_skia::Paint<'static> {
        tiny_skia::Paint {
            shader: tiny_skia::Shader::SolidColor(tc(color)),
            ..Default::default()
        }
    }

    fn fill(&mut self, path: &BezPath, at: &impl Fn(Point) -> Point, color: Color) {
        if let Some(p) = to_ts(path, at) {
            self.pm.fill_path(
                &p,
                &Self::paint(color),
                tiny_skia::FillRule::Winding,
                tiny_skia::Transform::default(),
                None,
            );
        }
    }

    fn stroke(&mut self, path: &BezPath, at: &impl Fn(Point) -> Point, color: Color, width: f64) {
        if let Some(p) = to_ts(path, at) {
            let mut stroke = tiny_skia::Stroke::default();
            stroke.width = width.max(0.05) as f32;
            self.pm.stroke_path(
                &p,
                &Self::paint(color),
                &stroke,
                tiny_skia::Transform::default(),
                None,
            );
        }
    }

    fn fill_circle(&mut self, centre: Point, r: f64, color: Color) {
        let path = BezPath::from_vec(
            Circle::new(centre, r.max(0.05))
                .path_elements(0.01)
                .collect(),
        );
        self.fill(&path, &|p| p, color);
    }

    fn fill_rect(&mut self, rect: Rect, color: Color) {
        let path = BezPath::from_vec(rect.path_elements(0.1).collect());
        self.fill(&path, &|p| p, color);
    }

    fn stroke_rect(&mut self, rect: Rect, color: Color, width: f64) {
        let path = BezPath::from_vec(rect.path_elements(0.1).collect());
        self.stroke(&path, &|p| p, color, width);
    }

    fn save(self, path: &str) {
        self.pm.save_png(path).expect("save png");
        println!("wrote {path}");
    }
}

// ---------------------------------------------------------------------------
// The figure, exactly as draw_figure does it
// ---------------------------------------------------------------------------

fn draw_figure(sheet: &mut Sheet, pose: &f::Pose, at: &impl Fn(Point) -> Point, scale: f64) {
    let edge = (scale * 0.035).max(0.5);
    for path in f::figure_paths(pose) {
        sheet.fill(&path, at, IRON);
        sheet.stroke(&path, at, RIM.with_alpha(0.85), edge);
    }
}

fn draw_fish(sheet: &mut Sheet, centre: Point, scale: f64, size: f64) {
    let body = Ellipse::new(centre, (scale * 0.075 * size, scale * 0.042 * size), 0.0);
    let path = BezPath::from_vec(body.path_elements(0.02).collect());
    sheet.fill(&path, &|p| p, FISH);
    let tail = Point::new(centre.x - scale * 0.075 * size, centre.y);
    let mut fin = BezPath::new();
    fin.move_to(tail);
    fin.line_to(Point::new(tail.x - scale * 0.045 * size, tail.y - scale * 0.035 * size));
    fin.line_to(Point::new(tail.x - scale * 0.045 * size, tail.y + scale * 0.035 * size));
    fin.close_path();
    sheet.fill(&fin, &|p| p, FISH);
}

// ---------------------------------------------------------------------------
// Sheet 1: the pose lineup
// ---------------------------------------------------------------------------

fn poses_sheet() {
    // Figure-height in px, generous so the silhouette can be judged.
    let s = 150.0;
    // Poses range from x -0.7 (shouldered rod) to 1.6 (cast rod tip).
    let (x0, x1) = (-0.75, 1.70);
    let cell_w = ((x1 - x0) * s) as u32 + 24;
    let cell_h = (1.15 * s) as u32 + 12;
    let cols = 4u32;

    // (label, doing, progress, phase) — phase chosen mid-gesture.
    let lineup: Vec<(&str, Doing, f64, f64)> = vec![
        ("sleeping", Doing::Sleeping, 0.5, 0.0),
        ("waking", Doing::Waking, 0.45, 0.0),
        ("exercising (down)", Doing::Exercising, 0.5, 0.10),
        ("exercising (up)", Doing::Exercising, 0.5, 0.31),
        ("coffee (sip)", Doing::Coffee, 0.5, 0.5),
        ("gardening", Doing::Gardening, 0.5, 0.45),
        ("fishing", Doing::Fishing, 0.5, 0.0),
        ("cooking", Doing::Cooking, 0.5, 0.0),
        ("eating (bite)", Doing::Eating, 0.5, 0.31),
        ("siesta", Doing::Siesta, 0.5, 0.0),
        ("reading", Doing::Reading, 0.5, 0.0),
        ("smoking (drag)", Doing::Smoking, 0.5, 0.63),
    ];

    let rows = (lineup.len() as u32).div_ceil(cols);
    let mut sheet = Sheet::new(cell_w * cols, cell_h * rows, STEEL_DEEP);

    for (i, (label, doing, progress, phase)) in lineup.iter().enumerate() {
        let (col, row) = (i as u32 % cols, i as u32 / cols);
        let ox = (col * cell_w) as f64 + 12.0;
        let oy = (row * cell_h) as f64 + 6.0;
        let at = |p: Point| Point::new(ox + (p.x - x0) * s, oy + p.y * s);

        let mut pose = f::breathe(
            f::pose_for(*doing, *progress, *phase),
            f::secondary(1.0, *doing),
        );
        pose.hat_tilt += f::head_drift(1.0);

        draw_figure(&mut sheet, &pose, &at, s);

        // The props each activity carries, simplified from draw_props.
        match doing {
            Doing::Fishing => {
                let mut rod = BezPath::new();
                rod.move_to(at(pose.rod_butt));
                rod.line_to(at(pose.rod_tip));
                sheet.stroke(&rod, &|p| p, IRON, (s * 0.06).max(1.2));
                sheet.stroke(&rod, &|p| p, RIM.with_alpha(0.9), (s * 0.025).max(0.6));
                let line = f::line_path(at(pose.rod_tip), oy + 1.12 * s, s * 0.04);
                sheet.stroke(&line, &|p| p, LINE.with_alpha(0.75), 0.9);
            }
            Doing::Coffee => {
                sheet.fill_circle(at(pose.hand), s * 0.055, PAGE.with_alpha(0.9));
            }
            Doing::Smoking => {
                sheet.fill_circle(
                    at(Point::new(pose.hand.x + 0.02, pose.hand.y - 0.02)),
                    (s * 0.05).max(0.9),
                    FIRE_CORE.with_alpha(0.85),
                );
            }
            Doing::Eating => {
                draw_fish(&mut sheet, at(pose.hand), s, 0.8);
            }
            Doing::Cooking => {
                draw_fish(&mut sheet, at(Point::new(0.74, 0.72)), s, 1.0);
            }
            _ => {}
        }
        println!("cell {i}: {label}");
    }

    sheet.save("/tmp/fisherman/poses.png");
}

// ---------------------------------------------------------------------------
// Sheet 2: the walk, frame by frame
// ---------------------------------------------------------------------------

fn walk_sheet() {
    let s = 150.0;
    let frames = 10u32;
    let cell_w = (2.45 * s) as u32 + 16;
    let cell_h = (1.15 * s) as u32 + 12;
    let mut sheet = Sheet::new(cell_w * frames, cell_h, STEEL_DEEP);

    for i in 0..frames {
        let progress = i as f64 / frames as f64;
        let ox = (i * cell_w) as f64 + 8.0;
        let at = |p: Point| Point::new(ox + (p.x + 0.75) * s, 6.0 + p.y * s);
        let pose = f::breathe(
            f::pose_for(Doing::Walking, progress, 0.0),
            f::secondary(progress * 12.0, Doing::Walking),
        );
        draw_figure(&mut sheet, &pose, &at, s);
        // The shouldered rod, as draw_line_and_rod now draws it on walks.
        let mut rod = BezPath::new();
        rod.move_to(at(pose.rod_butt));
        rod.line_to(at(pose.rod_tip));
        sheet.stroke(&rod, &|p| p, IRON, (s * 0.06).max(1.2));
        sheet.stroke(&rod, &|p| p, RIM.with_alpha(0.9), (s * 0.025).max(0.6));
    }
    sheet.save("/tmp/fisherman/walk.png");
}

// ---------------------------------------------------------------------------
// Sheet 3: the rail, as the app draws it
// ---------------------------------------------------------------------------

/// Replicates `draw_hut`, `draw_window`, smoke, fire and the plank walks at a
/// given band height, against the frame's steel.
#[allow(clippy::too_many_arguments)]
fn draw_scene(
    sheet: &mut Sheet,
    origin: Point,
    w: f64,
    band: f64,
    doing: Doing,
    place: Place,
    progress: f64,
    completion: f64,
    frame: u64,
    seconds: f64,
) {
    let h = band * 3.0; // the canvas is taller than the rail; he lives at its base
    let at0 = |p: Point| Point::new(origin.x + p.x, origin.y + p.y);

    // The rail itself, so contrast is judged against the real backdrop.
    sheet.fill_rect(
        Rect::new(origin.x, origin.y + h - band, origin.x + w, origin.y + h),
        STEEL_BODY,
    );

    let scale = band * 0.80;
    let stage_left = band * 1.5;
    let stage = (w - stage_left - band * 1.5 - scale * (1.0 + f::ROD_REACH)).max(1.0);
    let top = h - band + (band - scale) * 0.55;

    let hut = f::HutGeometry::new(stage_left - scale * 0.35, h - band * 0.10, scale * 1.45, band);

    // --- the hut, plank by plank (mirrors draw_hut) ---
    let (planks, roofed, chimney, doored, _lamp) = f::build_stage(completion);
    let (hl, hb, hw, hh) = (hut.left, hut.base, hut.width, hut.height);
    let edge = (hh * 0.04).max(0.5);
    let rect_at = |r: Rect| {
        let o = at0(r.origin());
        let c = at0(Point::new(r.x1, r.y1));
        Rect::new(o.x, o.y, c.x, c.y)
    };
    for plank in 0..planks {
        let t = hb - hh * (plank as f64 + 1.0) / PLANKS as f64;
        let b = hb - hh * plank as f64 / PLANKS as f64;
        let board = rect_at(Rect::new(hl, t, hl + hw, b));
        sheet.fill_rect(board, HUT_WALL);
        sheet.stroke_rect(board, RIM.with_alpha(0.30), edge * 0.7);
    }
    if roofed {
        let mut roof = BezPath::new();
        roof.move_to(Point::new(hl - hw * 0.10, hb - hh));
        roof.line_to(Point::new(hl + hw * 0.5, hb - hh * 1.34));
        roof.line_to(Point::new(hl + hw * 1.10, hb - hh));
        roof.close_path();
        sheet.fill(&roof, &at0, HUT_ROOF);
        sheet.stroke(&roof, &at0, RIM.with_alpha(0.6), edge);
    }
    if chimney {
        let stack = rect_at(Rect::new(
            hl + hw * 0.18,
            hb - hh * 1.02,
            hl + hw * 0.30,
            hb - hh * 0.80,
        ));
        sheet.fill_rect(stack, HUT_ROOF);
        sheet.stroke_rect(stack, RIM.with_alpha(0.5), edge * 0.8);
    }

    // Door openness off the routine (he is mid-walk in these scenes).
    let came_from = Place::Garden;
    let door_open = if completion < 1.0 {
        0.0
    } else {
        f::door_openness(doing, place, came_from, progress)
    };
    if doored {
        let (x0, x1) = (hl + hw * 0.30, hl + hw * 0.46);
        let top = hb - hh * 0.52;
        let o = at0(Point::new(x0, top));
        sheet.fill_rect(rect_at(Rect::new(x0, top, x1, hb)), DOORWAY);
        let spill = f::door_glow(f::window_light(doing, place, progress), door_open);
        if spill > 0.01 {
            sheet.fill_rect(
                rect_at(Rect::new(x0, top, x1, hb)),
                LAMP.with_alpha(spill as f32),
            );
        }
        let panel = x1 - (x1 - x0) * door_open;
        if panel > x0 {
            let p = at0(Point::new(panel, hb));
            sheet.fill_rect(Rect::new(o.x, o.y, p.x, p.y), HUT_ROOF);
            sheet.stroke_rect(Rect::new(o.x, o.y, p.x, p.y), RIM.with_alpha(0.45), edge * 0.6);
        }
    }

    // Window and smoke appear once the lamp stage is reached — draw_hut gates
    // both on the same flag.
    let (_, _, _, _, lamp) = f::build_stage(completion);
    if lamp {
        // --- window, mirrors draw_window ---
        let lit = f::window_light(doing, place, progress);
        let pane = rect_at(hut.window());
        if lit < 0.02 {
            sheet.fill_rect(pane, Color::from_rgb8(11, 13, 18));
            sheet.stroke_rect(pane, RIM.with_alpha(0.30), (hh * 0.02).max(0.4));
        } else {
            sheet.fill_rect(
                Rect::new(
                    pane.x0 - hh * 0.10,
                    pane.y0 - hh * 0.10,
                    pane.x1 + hh * 0.10,
                    pane.y1 + hh * 0.10,
                ),
                LAMP_DEEP.with_alpha(0.16 * lit as f32),
            );
            sheet.fill_rect(pane, LAMP.with_alpha((0.55 + 0.4 * lit) as f32));
            if place == Place::Hut && !matches!(doing, Doing::Sleeping | Doing::Walking) {
                let cx0 = pane.x0 + pane.width() * 0.55;
                let head = pane.y0 + pane.height() * 0.34;
                sheet.fill_circle(Point::new(cx0, head), pane.height() * 0.15, HUT_ROOF);
                sheet.fill_rect(
                    Rect::new(
                        cx0 - pane.width() * 0.22,
                        head + pane.height() * 0.12,
                        cx0 + pane.width() * 0.20,
                        pane.y1,
                    ),
                    HUT_ROOF,
                );
                if doing == Doing::Reading {
                    sheet.fill_rect(
                        Rect::new(
                            cx0 - pane.width() * 0.44,
                            head + pane.height() * 0.22,
                            cx0 - pane.width() * 0.16,
                            head + pane.height() * 0.52,
                        ),
                        PAGE.with_alpha(0.85),
                    );
                }
            }
            let mid = pane.x0 + pane.width() * 0.5;
            let mut bar = BezPath::new();
            bar.move_to(Point::new(mid, pane.y0));
            bar.line_to(Point::new(mid, pane.y1));
            sheet.stroke(&bar, &|p| p, HUT_ROOF, (hh * 0.03).max(0.6));
            sheet.stroke_rect(pane, RIM.with_alpha(0.55), (hh * 0.025).max(0.5));
        }

        // --- chimney smoke ---
        let strength = f::chimney_smoke(doing, place);
        if strength >= 0.05 {
            let mouth = at0(hut.chimney());
            for puff in 0..4 {
                let ph = ((frame as f64 / 46.0) + f64::from(puff) * 0.25).fract();
                let rise = ph * hh * 0.9;
                let drift = ph * ph * hh * 0.30;
                sheet.fill_circle(
                    Point::new(mouth.x + drift, mouth.y - rise),
                    (hh * 0.05).max(0.7) * (0.6 + ph * 1.9),
                    SMOKE.with_alpha((1.0 - ph) as f32 * 0.36 * strength as f32),
                );
            }
        }
    }

    // --- him ---
    let building = completion < 1.0;
    let along = if building {
        f::build_position(completion)
    } else {
        f::place_position(place)
    };
    let left = stage_left + along * stage;
    let at = move |p: Point| at0(Point::new(left + p.x * scale, top + p.y * scale));

    let phase = seconds / 6.0;
    let walk_progress = if building { completion * 8.0 } else { progress };
    let actual_doing = if building { Doing::Walking } else { doing };
    let mut pose = f::breathe(
        f::pose_for(actual_doing, walk_progress, phase),
        f::secondary(seconds, actual_doing),
    );
    pose.hat_tilt += f::head_drift(seconds);

    // Fire under him when cooking/eating.
    let strength = match actual_doing {
        Doing::Cooking => 1.0,
        Doing::Eating => 0.6,
        _ => 0.0,
    };
    if strength > 0.02 {
        let base = at(Point::new(0.80, 0.92));
        let height = scale * 0.38 * strength;
        // Hearth glow first, as in draw_fire.
        let glow = Ellipse::new(base, (scale * 0.30, scale * 0.09), 0.0);
        let glow_path = BezPath::from_vec(glow.path_elements(0.02).collect());
        sheet.fill(&glow_path, &|p| p, FIRE_BODY.with_alpha(0.22 * strength as f32));
        for (index, colour, spread) in [
            (0usize, FIRE_DEEP, 1.0f64),
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
            sheet.fill(&flame, &|p| p, colour.with_alpha(0.92));
        }
        // Sparks.
        for spark in 0..3 {
            let p = ((frame as f64 / 18.0) + f64::from(spark) * 0.37).fract();
            let wander = (f64::from(spark) - 1.0) * scale * 0.05 + p * scale * 0.03;
            sheet.fill_circle(
                Point::new(base.x + wander, base.y - height * (0.5 + p * 0.9)),
                (scale * 0.018).max(0.4),
                FIRE_CORE.with_alpha((1.0 - p) as f32 * 0.8 * strength as f32),
            );
        }
    }

    let at_the_water =
        matches!(actual_doing, Doing::Fishing | Doing::Smoking) && place == Place::Perch;
    // Never while building, as in the app: then he carries planks, not the rod.
    if !building && (at_the_water || actual_doing == Doing::Walking) {
        let mut rod = BezPath::new();
        rod.move_to(at(pose.rod_butt));
        rod.line_to(at(pose.rod_tip));
        sheet.stroke(&rod, &|p| p, IRON, (scale * 0.06).max(1.2));
        sheet.stroke(&rod, &|p| p, RIM.with_alpha(0.9), (scale * 0.025).max(0.6));
        if at_the_water {
            let sway = (((seconds - 0.4) / 5.3) * std::f64::consts::TAU).sin() * scale * 0.08;
            let line = f::line_path(at(pose.rod_tip), origin.y + h - 2.0, sway);
            sheet.stroke(&line, &|p| p, LINE.with_alpha(0.75), 0.9);
        }
    }

    draw_figure(sheet, &pose, &at, scale);

    // The plank, while building.
    if building {
        let trip = (completion * (PLANKS as f64 + 2.0)).fract();
        if trip <= 0.5 {
            let hand = at(pose.hand);
            sheet.fill_rect(
                Rect::new(
                    hand.x - scale * 0.34,
                    hand.y - scale * 0.04,
                    hand.x + scale * 0.10,
                    hand.y + scale * 0.02,
                ),
                HUT_WALL,
            );
        }
    }

    // Props.
    match actual_doing {
        Doing::Coffee => {
            let mug = at(pose.hand);
            sheet.fill_circle(mug, scale * 0.055, PAGE.with_alpha(0.9));
        }
        Doing::Smoking => {
            let ember = at(Point::new(pose.hand.x + 0.02, pose.hand.y - 0.02));
            sheet.fill_circle(ember, (scale * 0.05).max(0.9), FIRE_CORE.with_alpha(0.85));
        }
        Doing::Gardening => {
            for row in 0..4 {
                let x = 0.15 + f64::from(row) * 0.20;
                let mut shoot = BezPath::new();
                let base = at(Point::new(x, 0.95));
                shoot.move_to(base);
                shoot.quad_to(
                    Point::new(base.x - scale * 0.02, base.y - scale * 0.06),
                    Point::new(base.x + scale * 0.03, base.y - scale * 0.10),
                );
                sheet.stroke(&shoot, &|p| p, GREEN.with_alpha(0.8), (scale * 0.02).max(0.5));
            }
        }
        Doing::Eating => {
            draw_fish(sheet, at(pose.hand), scale, 0.6);
        }
        _ => {}
    }
}

fn scene_sheet(band: f64, name: &str) {
    // Each tile is a full-width rail slice; stacked vertically.
    let tile_w = 1100.0;
    let tile_h = band * 3.0;
    let gap = 8.0;

    // (label, doing, place, progress, completion, frame, seconds)
    let tiles: Vec<(&str, Doing, Place, f64, f64, u64, f64)> = vec![
        ("build 20%", Doing::Walking, Place::Garden, 0.0, 0.20, 40, 8.0),
        ("build 55%", Doing::Walking, Place::Garden, 0.0, 0.55, 40, 8.0),
        ("build 85%", Doing::Walking, Place::Garden, 0.0, 0.85, 40, 8.0),
        (
            "built, morning coffee on the doorstep",
            Doing::Coffee,
            Place::Doorstep,
            0.5,
            1.0,
            40,
            8.0,
        ),
        ("gardening", Doing::Gardening, Place::Garden, 0.5, 1.0, 40, 9.0),
        (
            "fishing at the perch",
            Doing::Fishing,
            Place::Perch,
            0.5,
            1.0,
            40,
            10.0,
        ),
        (
            "cooking at the fire",
            Doing::Cooking,
            Place::Fire,
            0.5,
            1.0,
            40,
            18.0,
        ),
        (
            "walking home, door opening",
            Doing::Walking,
            Place::Hut,
            0.92,
            1.0,
            40,
            20.0,
        ),
        (
            "reading by lamplight",
            Doing::Reading,
            Place::Hut,
            0.5,
            1.0,
            60,
            22.0,
        ),
        (
            "asleep, lamp out",
            Doing::Sleeping,
            Place::Hut,
            0.5,
            1.0,
            60,
            23.5,
        ),
    ];

    let mut sheet = Sheet::new(
        tile_w as u32,
        (tiles.len() as f64 * (tile_h + gap)) as u32,
        STEEL_DEEP,
    );
    for (i, (label, doing, place, progress, completion, frame, seconds)) in tiles.iter().enumerate()
    {
        println!("tile {i}: {label}");
        draw_scene(
            &mut sheet,
            Point::new(0.0, i as f64 * (tile_h + gap)),
            tile_w,
            band,
            *doing,
            *place,
            *progress,
            *completion,
            *frame,
            *seconds,
        );
    }
    sheet.save(name);
}

fn main() {
    std::fs::create_dir_all("/tmp/fisherman").expect("mkdir");
    poses_sheet();
    walk_sheet();
    scene_sheet(44.0, "/tmp/fisherman/scene_1x.png");
    scene_sheet(132.0, "/tmp/fisherman/scene_3x.png");
}
