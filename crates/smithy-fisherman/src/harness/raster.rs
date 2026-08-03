//! Rasterising kurbo paths through [`Ink`] onto a `tiny_skia::Pixmap`.
//!
//! Lifted from the preview example so checks and sheets share one path —
//! a second rasteriser would be the original seam bug in miniature.
//!
//! The part mask is the reason `Ink::begin` exists: IRON cannot be told from
//! hut AA by colour (midpoint of HUT_ROOF→HUT_WALL is distance 2 from IRON),
//! so the harness records what `paint` said it was drawing.

use kurbo::{BezPath, PathEl, Point, Rect, Shape};
use peniko::Color;

use crate::fisherman::{self as f, Scene};
use crate::{Ink, Part};

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

/// A pixmap that implements [`Ink`], with an exact per-[`Part`] mask.
pub struct PixmapInk {
    pub pm: tiny_skia::Pixmap,
    /// Parallel buffer: R = [`Part::tag`], A = 255 when stamped. No AA, so
    /// A is binary — coverage blending would average tags into phantom Parts.
    mask: tiny_skia::Pixmap,
    current: Option<Part>,
}

impl PixmapInk {
    pub fn new(w: u32, h: u32, bg: Color) -> Self {
        let mut pm = tiny_skia::Pixmap::new(w, h).expect("pixmap");
        pm.fill(tc(bg));
        let mut mask = tiny_skia::Pixmap::new(w, h).expect("mask");
        mask.fill(tiny_skia::Color::from_rgba8(0, 0, 0, 0));
        PixmapInk {
            pm,
            mask,
            current: None,
        }
    }

    /// Colour-only load for golden comparison — mask stays empty.
    pub fn from_pixmap(pm: tiny_skia::Pixmap) -> Self {
        let mut mask = tiny_skia::Pixmap::new(pm.width(), pm.height()).expect("mask");
        mask.fill(tiny_skia::Color::from_rgba8(0, 0, 0, 0));
        PixmapInk {
            pm,
            mask,
            current: None,
        }
    }

    fn paint(color: Color) -> tiny_skia::Paint<'static> {
        tiny_skia::Paint {
            shader: tiny_skia::Shader::SolidColor(tc(color)),
            ..Default::default()
        }
    }

    fn mask_paint(part: Part) -> tiny_skia::Paint<'static> {
        let t = part.tag();
        tiny_skia::Paint {
            // No AA on the mask. tiny-skia's Source still coverage-blends
            // into the destination on antialiased edges, so Hut(1) under
            // Smoke(6) at cov≈0.2 writes R=2 — which is Part::Figure. That
            // made every indoor frame report four phantom figure pixels
            // (begin(Figure) never ran). Tags are labels: covered or not.
            // Colour pixmap keeps AA; only the mask is binary.
            shader: tiny_skia::Shader::SolidColor(tiny_skia::Color::from_rgba8(t, 0, 0, 255)),
            blend_mode: tiny_skia::BlendMode::Source,
            anti_alias: false,
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

    /// Which [`Part`] owns this pixel, if any.
    pub fn part_at(&self, x: u32, y: u32) -> Option<Part> {
        let i = ((y * self.mask.width() + x) * 4) as usize;
        let d = self.mask.data();
        if i + 3 >= d.len() {
            return None;
        }
        // A > 0 means the mask was stamped; R holds the tag.
        if d[i + 3] == 0 {
            return None;
        }
        Part::from_tag(d[i])
    }

    /// Bounding box of pixels tagged as `part`.
    pub fn part_bounds(&self, part: Part) -> Option<(u32, u32, u32, u32)> {
        let tag = part.tag();
        let mut min_x = self.width();
        let mut min_y = self.height();
        let mut max_x = 0u32;
        let mut max_y = 0u32;
        let mut any = false;
        let d = self.mask.data();
        let w = self.width();
        for y in 0..self.height() {
            for x in 0..w {
                let i = ((y * w + x) * 4) as usize;
                if d[i + 3] == 0 || d[i] != tag {
                    continue;
                }
                any = true;
                min_x = min_x.min(x);
                min_y = min_y.min(y);
                max_x = max_x.max(x);
                max_y = max_y.max(y);
            }
        }
        any.then_some((min_x, min_y, max_x, max_y))
    }

    /// Count of pixels tagged as `part`.
    pub fn part_count(&self, part: Part) -> u64 {
        let tag = part.tag();
        let d = self.mask.data();
        let mut n = 0u64;
        let mut i = 0;
        while i + 3 < d.len() {
            if d[i + 3] != 0 && d[i] == tag {
                n += 1;
            }
            i += 4;
        }
        n
    }

    /// Bounding box of non-background colour ink (any part). Fallback when
    /// neither figure nor hut was tagged — should be rare after `begin`.
    pub fn content_bounds(&self) -> Option<(u32, u32, u32, u32)> {
        let mut min_x = self.width();
        let mut min_y = self.height();
        let mut max_x = 0u32;
        let mut max_y = 0u32;
        let mut any = false;
        for y in 0..self.height() {
            for x in 0..self.width() {
                let Some((r, g, b, _)) = self.pixel(x, y) else {
                    continue;
                };
                if is_bg((r, g, b)) {
                    continue;
                }
                any = true;
                min_x = min_x.min(x);
                min_y = min_y.min(y);
                max_x = max_x.max(x);
                max_y = max_y.max(y);
            }
        }
        any.then_some((min_x, min_y, max_x, max_y))
    }

    /// Copy a source rectangle into this pixmap at `(dst_x, dst_y)`.
    /// Colour only — sheet composites do not need the per-part mask.
    pub fn blit_from(
        &mut self,
        src: &PixmapInk,
        src_x: u32,
        src_y: u32,
        w: u32,
        h: u32,
        dst_x: u32,
        dst_y: u32,
    ) {
        for row in 0..h {
            for col in 0..w {
                let sx = src_x + col;
                let sy = src_y + row;
                let dx = dst_x + col;
                let dy = dst_y + row;
                if sx >= src.width() || sy >= src.height() || dx >= self.width() || dy >= self.height()
                {
                    continue;
                }
                let Some((r, g, b, a)) = src.pixel(sx, sy) else {
                    continue;
                };
                let i = ((dy * self.width() + dx) * 4) as usize;
                let d = self.pm.data_mut();
                d[i] = r;
                d[i + 1] = g;
                d[i + 2] = b;
                d[i + 3] = a;
            }
        }
    }

    fn stamp_mask(&mut self, path: &tiny_skia::Path, stroke: Option<&tiny_skia::Stroke>) {
        let Some(part) = self.current else {
            return;
        };
        let paint = Self::mask_paint(part);
        if let Some(stroke) = stroke {
            self.mask.stroke_path(
                path,
                &paint,
                stroke,
                tiny_skia::Transform::default(),
                None,
            );
        } else {
            self.mask.fill_path(
                path,
                &paint,
                tiny_skia::FillRule::Winding,
                tiny_skia::Transform::default(),
                None,
            );
        }
    }
}

impl Ink for PixmapInk {
    fn begin(&mut self, part: Part) {
        self.current = Some(part);
    }

    fn fill(&mut self, path: &BezPath, color: Color) {
        if let Some(p) = to_ts(path) {
            self.pm.fill_path(
                &p,
                &Self::paint(color),
                tiny_skia::FillRule::Winding,
                tiny_skia::Transform::default(),
                None,
            );
            self.stamp_mask(&p, None);
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
            self.stamp_mask(&p, Some(&stroke));
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
    fn begin(&mut self, part: Part) {
        self.inner.begin(part);
    }

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
    // No begin — steel is backdrop, not a Part.
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

/// RIM / RIM_BRIGHT stroke pixels (gold edge). Used by the contrast check —
/// that one genuinely wants a colour test against the steel behind him.
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
