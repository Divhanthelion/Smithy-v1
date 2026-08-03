//! Headless renderer for the fisherman: draws him to PNGs so his silhouette
//! can be *looked at* rather than tuned blind.
//!
//!     cargo run -p smithy-fisherman --example fisherman_preview
//!
//! Writes `/tmp/fisherman/*.png`:
//!   poses.png   — every activity, large, in a labelled grid order (see stdout)
//!   walk.png    — a full stride, frame by frame
//!   scene_1x.png / scene_3x.png — the whole rail at true size and zoomed:
//!                 build stages, door beats, fire, window light, the works
//!
//! Scene sheets call [`fisherman::paint`] directly. Poses/walk sheets are
//! isolation grids: pose APIs + palette + Ink, not a second copy of the hut.

use kurbo::{BezPath, Circle, Ellipse, PathEl, Point, Rect, Shape};
use peniko::Color;

use smithy_fisherman::fisherman::{self as f, Scene};
use smithy_fisherman::routine::{Doing, Place};
use smithy_fisherman::Ink;

// The frame's steel, from forged.rs, as a backdrop to judge contrast against.
// Not fisherman palette — it is the rail he stands on.
const STEEL_DEEP: Color = Color::from_rgb8(20, 24, 34);
const STEEL_BODY: Color = Color::from_rgb8(41, 49, 66);

// ---------------------------------------------------------------------------
// Rasterising kurbo paths with tiny-skia
// ---------------------------------------------------------------------------

fn tc(c: Color) -> tiny_skia::Color {
    let c = c.to_rgba8();
    tiny_skia::Color::from_rgba8(c.r, c.g, c.b, c.a)
}

fn to_ts(path: &BezPath) -> Option<tiny_skia::Path> {
    let mut pb = tiny_skia::PathBuilder::new();
    for el in path.elements() {
        match *el {
            PathEl::MoveTo(p) => pb.move_to(p.x as f32, p.y as f32),
            PathEl::LineTo(p) => pb.line_to(p.x as f32, p.y as f32),
            PathEl::QuadTo(a, b) => pb.quad_to(a.x as f32, a.y as f32, b.x as f32, b.y as f32),
            PathEl::CurveTo(a, b, c) => {
                pb.cubic_to(
                    a.x as f32,
                    a.y as f32,
                    b.x as f32,
                    b.y as f32,
                    c.x as f32,
                    c.y as f32,
                );
            }
            PathEl::ClosePath => pb.close(),
        }
    }
    pb.finish()
}

struct PixmapInk {
    pm: tiny_skia::Pixmap,
}

impl PixmapInk {
    fn new(w: u32, h: u32, bg: Color) -> Self {
        let mut pm = tiny_skia::Pixmap::new(w, h).expect("pixmap");
        pm.fill(tc(bg));
        PixmapInk { pm }
    }

    fn paint(color: Color) -> tiny_skia::Paint<'static> {
        tiny_skia::Paint {
            shader: tiny_skia::Shader::SolidColor(tc(color)),
            ..Default::default()
        }
    }

    fn save(self, path: &str) {
        self.pm.save_png(path).expect("save png");
        println!("wrote {path}");
    }
}

impl Ink for PixmapInk {
    fn fill(&mut self, path: &BezPath, color: Color) {
        if let Some(p) = to_ts(path) {
            self.pm.fill_path(
                &p,
                &Self::paint(color),
                tiny_skia::FillRule::Winding,
                tiny_skia::Transform::default(),
                None,
            );
        }
    }

    fn stroke(&mut self, path: &BezPath, color: Color, width: f64) {
        if let Some(p) = to_ts(path) {
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
}

/// Translate ink calls so a Scene can be painted into a tile of a larger sheet.
struct OffsetInk<'a> {
    inner: &'a mut PixmapInk,
    dx: f64,
    dy: f64,
}

impl Ink for OffsetInk<'_> {
    fn fill(&mut self, path: &BezPath, color: Color) {
        self.inner.fill(&shift(path, self.dx, self.dy), color);
    }

    fn stroke(&mut self, path: &BezPath, color: Color, width: f64) {
        self.inner
            .stroke(&shift(path, self.dx, self.dy), color, width);
    }
}

fn shift(path: &BezPath, dx: f64, dy: f64) -> BezPath {
    let mut out = BezPath::new();
    for el in path.elements() {
        match *el {
            PathEl::MoveTo(p) => out.move_to(Point::new(p.x + dx, p.y + dy)),
            PathEl::LineTo(p) => out.line_to(Point::new(p.x + dx, p.y + dy)),
            PathEl::QuadTo(a, b) => out.quad_to(
                Point::new(a.x + dx, a.y + dy),
                Point::new(b.x + dx, b.y + dy),
            ),
            PathEl::CurveTo(a, b, c) => out.curve_to(
                Point::new(a.x + dx, a.y + dy),
                Point::new(b.x + dx, b.y + dy),
                Point::new(c.x + dx, c.y + dy),
            ),
            PathEl::ClosePath => out.close_path(),
        }
    }
    out
}

/// Isolation-sheet figure: same treatment as `draw_figure`, without owning a copy.
fn ink_figure(ink: &mut impl Ink, pose: &f::Pose, at: &impl Fn(Point) -> Point, scale: f64) {
    let edge = (scale * 0.035).max(0.5);
    for path in f::figure_paths(pose) {
        let placed = place(&path, at);
        ink.fill(&placed, f::IRON);
        ink.stroke(&placed, f::RIM.with_alpha(0.85), edge);
    }
}

fn place(path: &BezPath, at: &impl Fn(Point) -> Point) -> BezPath {
    let mut out = BezPath::new();
    for element in path.elements() {
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
    let mut sheet = PixmapInk::new(cell_w * cols, cell_h * rows, STEEL_DEEP);

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

        ink_figure(&mut sheet, &pose, &at, s);

        // Isolation props: enough to read the gesture, not a second draw_props.
        match doing {
            Doing::Fishing => {
                let mut rod = BezPath::new();
                rod.move_to(at(pose.rod_butt));
                rod.line_to(at(pose.rod_tip));
                sheet.stroke(&rod, f::IRON, (s * 0.06).max(1.2));
                sheet.stroke(&rod, f::RIM.with_alpha(0.9), (s * 0.025).max(0.6));
                let line = f::line_path(at(pose.rod_tip), oy + 1.12 * s, s * 0.04);
                sheet.stroke(&line, f::LINE.with_alpha(0.75), 0.9);
            }
            Doing::Coffee => {
                let mug = BezPath::from_vec(
                    Circle::new(at(pose.hand), s * 0.055)
                        .path_elements(0.25)
                        .collect(),
                );
                sheet.fill(&mug, f::PAGE.with_alpha(0.9));
            }
            Doing::Smoking => {
                let ember = BezPath::from_vec(
                    Circle::new(
                        at(Point::new(pose.hand.x + 0.02, pose.hand.y - 0.02)),
                        (s * 0.05).max(0.9),
                    )
                    .path_elements(0.25)
                    .collect(),
                );
                sheet.fill(&ember, f::FIRE_CORE.with_alpha(0.85));
            }
            Doing::Eating | Doing::Cooking => {
                // Isolation mark only — full fish lives in paint's draw_fish.
                let centre = if *doing == Doing::Cooking {
                    at(Point::new(0.74, 0.72))
                } else {
                    at(pose.hand)
                };
                let size = if *doing == Doing::Cooking { 1.0 } else { 0.8 };
                let body = BezPath::from_vec(
                    Ellipse::new(centre, (s * 0.075 * size, s * 0.042 * size), 0.0)
                        .path_elements(0.25)
                        .collect(),
                );
                sheet.fill(&body, f::FISH);
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
    let mut sheet = PixmapInk::new(cell_w * frames, cell_h, STEEL_DEEP);

    for i in 0..frames {
        let progress = i as f64 / frames as f64;
        let ox = (i * cell_w) as f64 + 8.0;
        let at = |p: Point| Point::new(ox + (p.x + 0.75) * s, 6.0 + p.y * s);
        let pose = f::breathe(
            f::pose_for(Doing::Walking, progress, 0.0),
            f::secondary(progress * 12.0, Doing::Walking),
        );
        ink_figure(&mut sheet, &pose, &at, s);
        let mut rod = BezPath::new();
        rod.move_to(at(pose.rod_butt));
        rod.line_to(at(pose.rod_tip));
        sheet.stroke(&rod, f::IRON, (s * 0.06).max(1.2));
        sheet.stroke(&rod, f::RIM.with_alpha(0.9), (s * 0.025).max(0.6));
    }
    sheet.save("/tmp/fisherman/walk.png");
}

// ---------------------------------------------------------------------------
// Sheet 3: the rail, via paint
// ---------------------------------------------------------------------------

fn scene_sheet(band: f64, name: &str) {
    let tile_w = 1100.0;
    let tile_h = band * 3.0;
    let gap = 8.0;

    // (label, doing, place, previous, progress, completion, frame, seconds)
    let tiles: &[(&str, Doing, Place, Place, f64, f64, u64, f64)] = &[
        ("build 20%", Doing::Walking, Place::Garden, Place::Garden, 0.0, 0.20, 40, 8.0),
        ("build 55%", Doing::Walking, Place::Garden, Place::Garden, 0.0, 0.55, 40, 8.0),
        ("build 85%", Doing::Walking, Place::Garden, Place::Garden, 0.0, 0.85, 40, 8.0),
        (
            "built, morning coffee on the doorstep",
            Doing::Coffee,
            Place::Doorstep,
            Place::Hut,
            0.5,
            1.0,
            40,
            8.0,
        ),
        ("gardening", Doing::Gardening, Place::Garden, Place::Doorstep, 0.5, 1.0, 40, 9.0),
        ("fishing at the perch", Doing::Fishing, Place::Perch, Place::Garden, 0.5, 1.0, 40, 10.0),
        ("cooking at the fire", Doing::Cooking, Place::Fire, Place::Perch, 0.5, 1.0, 40, 18.0),
        (
            "walking home, door opening",
            Doing::Walking,
            Place::Hut,
            Place::Garden,
            0.92,
            1.0,
            40,
            20.0,
        ),
        ("reading by lamplight", Doing::Reading, Place::Hut, Place::Doorstep, 0.5, 1.0, 60, 22.0),
        ("asleep, lamp out", Doing::Sleeping, Place::Hut, Place::Doorstep, 0.5, 1.0, 60, 23.5),
    ];

    let mut sheet = PixmapInk::new(
        tile_w as u32,
        (tiles.len() as f64 * (tile_h + gap)) as u32,
        STEEL_DEEP,
    );
    for (i, (label, doing, place, previous, progress, completion, frame, seconds)) in
        tiles.iter().enumerate()
    {
        println!("tile {i}: {label}");
        let origin_y = i as f64 * (tile_h + gap);
        // Rail backdrop — paint draws the figure, not the forged steel.
        let rail_path = BezPath::from_vec(
            Rect::new(0.0, tile_h - band, tile_w, tile_h)
                .path_elements(0.25)
                .collect(),
        );
        let mut tile = OffsetInk {
            inner: &mut sheet,
            dx: 0.0,
            dy: origin_y,
        };
        tile.fill(&rail_path, STEEL_BODY);

        let scene = Scene {
            width: tile_w,
            height: tile_h,
            band,
            doing: *doing,
            place: *place,
            previous: *previous,
            progress: *progress,
            completion: *completion,
            frame: *frame,
            seconds: *seconds,
        };
        f::paint(&mut tile, &scene);
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
