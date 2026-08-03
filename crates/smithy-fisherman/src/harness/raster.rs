//! Rasterising kurbo paths through [`Ink`] onto a `tiny_skia::Pixmap`.
//!
//! Lifted from the preview example so checks and sheets share one path —
//! a second rasteriser would be the original seam bug in miniature.

use kurbo::{BezPath, PathEl, Point, Rect, Shape};
use peniko::Color;

use crate::fisherman::{self as f, Scene};
use crate::Ink;

/// The frame's steel, from forged.rs — not fisherman palette. He stands on it;
/// contrast checks compare RIM against this, not against black.
pub const STEEL_DEEP: Color = Color::from_rgb8(20, 24, 34);
pub const STEEL_BODY: Color = Color::from_rgb8(41, 49, 66);

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

/// A pixmap that implements [`Ink`].
pub struct PixmapInk {
    pub pm: tiny_skia::Pixmap,
}

impl PixmapInk {
    pub fn new(w: u32, h: u32, bg: Color) -> Self {
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

    pub fn save(&self, path: &std::path::Path) {
        self.pm.save_png(path).expect("save png");
        eprintln!("wrote {}", path.display());
    }

    pub fn width(&self) -> u32 {
        self.pm.width()
    }

    pub fn height(&self) -> u32 {
        self.pm.height()
    }

    pub fn pixel(&self, x: u32, y: u32) -> Option<(u8, u8, u8, u8)> {
        let i = ((y * self.pm.width() + x) * 4) as usize;
        let d = self.pm.data();
        if i + 3 >= d.len() {
            return None;
        }
        Some((d[i], d[i + 1], d[i + 2], d[i + 3]))
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

/// Translate ink so a Scene paints into a tile of a larger sheet.
pub struct OffsetInk<'a> {
    pub inner: &'a mut PixmapInk,
    pub dx: f64,
    pub dy: f64,
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

/// Fill the rail band with steel, then [`f::paint`] the scene.
///
/// Paint does not draw the forged steel — without this fill he hangs in a
/// void and contrast checks have nothing to compare against.
pub fn render_scene(scene: &Scene) -> PixmapInk {
    let w = scene.width.round().max(1.0) as u32;
    let h = scene.height.round().max(1.0) as u32;
    let mut ink = PixmapInk::new(w, h, STEEL_DEEP);
    let rail = BezPath::from_vec(
        Rect::new(0.0, scene.height - scene.band, scene.width, scene.height)
            .path_elements(0.25)
            .collect(),
    );
    ink.fill(&rail, STEEL_BODY);
    f::paint(&mut ink, scene);
    ink
}

/// Colour helpers for pixel analysis. Exact match on solid fills; AA edges
/// are handled by the callers that tolerate a channel distance.

pub fn rgba8(c: Color) -> (u8, u8, u8, u8) {
    let c = c.to_rgba8();
    (c.r, c.g, c.b, c.a)
}

pub fn colour_dist(a: (u8, u8, u8), b: (u8, u8, u8)) -> i32 {
    (a.0 as i32 - b.0 as i32).abs()
        + (a.1 as i32 - b.1 as i32).abs()
        + (a.2 as i32 - b.2 as i32).abs()
}

pub fn is_bg(rgb: (u8, u8, u8)) -> bool {
    let deep = rgba8(STEEL_DEEP);
    let body = rgba8(STEEL_BODY);
    colour_dist(rgb, (deep.0, deep.1, deep.2)) <= 6
        || colour_dist(rgb, (body.0, body.1, body.2)) <= 6
}

/// Solid IRON fill of the outdoor figure (and the rod). Distinct from
/// [`HUT_ROOF`] used for the window silhouette — without that split the
/// "hidden indoors" check cannot tell drawn-on-rail from drawn-in-window.
///
/// Tight radius: AA fringes of HUT_WALL/HUT_ROOF land within ~6 of IRON and
/// are not figure ink. Solid figure fills are exact `(17,20,27)`; ≤3 catches
/// them and rejects the hut-edge false positives measured 2026-08-03
/// (indoors frames reported 3–4 "IRON" pixels that were plank AA).
pub fn is_iron(rgb: (u8, u8, u8)) -> bool {
    let iron = rgba8(f::IRON);
    colour_dist(rgb, (iron.0, iron.1, iron.2)) <= 3
}

/// RIM / RIM_BRIGHT stroke pixels (gold edge). Used by the contrast check.
pub fn is_rim(rgb: (u8, u8, u8)) -> bool {
    let rim = rgba8(f::RIM);
    let bright = rgba8(f::RIM_BRIGHT);
    // AA against steel pulls gold toward blue-grey; keep the radius wide
    // enough to catch stroke cores, tight enough to reject PAGE / LAMP.
    colour_dist(rgb, (rim.0, rim.1, rim.2)) <= 90
        || colour_dist(rgb, (bright.0, bright.1, bright.2)) <= 90
}

pub fn is_fire(rgb: (u8, u8, u8)) -> bool {
    let core = rgba8(f::FIRE_CORE);
    let body = rgba8(f::FIRE_BODY);
    let deep = rgba8(f::FIRE_DEEP);
    colour_dist(rgb, (core.0, core.1, core.2)) <= 60
        || colour_dist(rgb, (body.0, body.1, body.2)) <= 60
        || colour_dist(rgb, (deep.0, deep.1, deep.2)) <= 50
}

pub fn is_lamp_warm(rgb: (u8, u8, u8)) -> bool {
    let lamp = rgba8(f::LAMP);
    let deep = rgba8(f::LAMP_DEEP);
    colour_dist(rgb, (lamp.0, lamp.1, lamp.2)) <= 80
        || colour_dist(rgb, (deep.0, deep.1, deep.2)) <= 60
}

pub fn luminance(rgb: (u8, u8, u8)) -> f64 {
    0.2126 * rgb.0 as f64 + 0.7152 * rgb.1 as f64 + 0.0722 * rgb.2 as f64
}
