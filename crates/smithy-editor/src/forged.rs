//! The ornamented interface: a carved frame around the working editor.
//!
//! Drawn as chrome *behind* the shell rather than as an alternative view tree.
//! The obvious implementation — a `dyn_container` swapping between two whole
//! layouts — would tear down and rebuild the editor, terminal and agent panel
//! on every switch, losing open buffers, scrollback and the live session. The
//! frame is around the work, not instead of it, so it is painted beneath and
//! the shell is inset to sit within it.
//!
//! Everything here is procedural: gradients, bezels and bevels rather than
//! bitmaps. That keeps it resolution-independent and asset-free, at the cost
//! of the photographic grain a texture would give.

use floem::peniko::kurbo::{
    BezPath, Circle, CubicBez, ParamCurve, ParamCurveDeriv, Point, Rect, Stroke,
};
use floem::peniko::{Brush, Color, ColorStop, Gradient};
use floem::prelude::*;
use floem::reactive::{RwSignal, SignalGet};
use floem::text::{Attrs, AttrsList, FamilyOwned, FontWeight, TextLayout};

use crate::aesthetic::Aesthetic;
use crate::lsp::Severity;
use crate::problems_panel::ProblemRow;

/// How thick the frame is at the sides and bottom.
pub const FRAME_INSET: f32 = 44.0;
/// How much taller the frame is at the top, to seat the wordmark.
pub const HEADER_HEIGHT: f32 = 64.0;

const WORDMARK_FAMILY: &[FamilyOwned] = &[FamilyOwned::Serif];
const WORDMARK: &str = "SMITHY";
const WORDMARK_SIZE: f32 = 22.0;
/// Letterspacing, applied by drawing each glyph separately. A wide-tracked
/// serif carries far more presence than a tight one at this size, and tracking
/// is not exposed through the text attributes.
const WORDMARK_TRACKING: f64 = 7.0;

/// Radius of the opening the shell shows through.
const INNER_RADIUS: f64 = 4.0;

/// How far a stone's bezel ring stands outside the stone itself. Named because
/// the corner volutes have to spring from *outside* it: it is an opaque fill
/// drawn after them, so anything within it is painted and then covered.
const GEM_BEZEL: f64 = 1.6;
/// The stone set at each corner, where the frieze turns.
const CORNER_GEM_R: f64 = FRAME_INSET as f64 * 0.22;
/// The radius nothing drawn before the stone can survive inside.
const CORNER_GEM_OUTER: f64 = CORNER_GEM_R + GEM_BEZEL;

/// Starting radius of a corner volute — the wide end, at the corner.
///
/// Bounded by the frieze it rides, not by the frame: a volute of this radius
/// swings about `0.73 * r` clear of the rail's centre line at its widest, and
/// the frieze only gives it `0.175` of the frame's depth either side. Both
/// facts are asserted rather than trusted, in
/// `a_corner_volute_stays_on_the_frieze_it_rides`.
const VOLUTE_R: f64 = FRAME_INSET as f64 * 0.19;
/// How fast the volute closes. `r = r0 * e^(-decay * theta)`; over the
/// three-quarter turn it sweeps this brings the eye in to about a third.
const VOLUTE_DECAY: f64 = 0.22;
/// Width of the volute's inlay, which is also how far its paint spreads either
/// side of the path — so it counts against the frieze budget.
const VOLUTE_INLAY: f64 = 2.4;

/// One repeat of the vine, as a multiple of the band it rides. The classical
/// running-scroll unit.
const VINE_REPEAT: f64 = 1.5;
/// Everything else about the vine is a fraction of the frieze's *half*-width,
/// because that is the space the ornament actually has: it is measured either
/// side of the run's centre line. Sizing from the full width and then swinging
/// that far both ways is what put the first version's leaves off the member.
const VINE_AMPLITUDE: f64 = 0.30;
const VINE_STEM: f64 = 0.44;
const VINE_LEAF_LEN: f64 = 0.90;
const VINE_LEAF_WIDTH: f64 = 0.40;
/// How far forward a leaf leans, as a multiple of how far it stands out. A
/// leaf square to the stem eats the whole band; raking it forward buys length
/// back and reads as growth in the direction of the run rather than as a row
/// of paddles.
const VINE_LEAF_RAKE: f64 = 1.2;
const VINE_TENDRIL: f64 = 0.34;
const VINE_TENDRIL_DECAY: f64 = 0.30;
const VINE_BERRY: f64 = 0.20;
/// The tendril's inlay. Held well under its radius: a spiral stroked as wide
/// as it is round is a dot, which is the same resolving limit that gives the
/// stones their level-of-detail ladder. Sizing the vine to its member shrank
/// every part of it, and this is the part that shrank past legibility first.
const VINE_TENDRIL_INLAY: f64 = 1.5;

// Blued steel, lit hard from the top-left. Much darker and cooler than a first
// pass in warm browns, which read as tan mud rather than as metal: what makes
// something look metallic is not the hue but the *contrast ratio* between a
// dark body and a very tight, very bright specular.
const STEEL_VOID: Color = Color::from_rgb8(9, 11, 16);
const STEEL_DEEP: Color = Color::from_rgb8(20, 24, 34);
const STEEL_BODY: Color = Color::from_rgb8(41, 49, 66);
const STEEL_FACE: Color = Color::from_rgb8(78, 91, 116);
const STEEL_RIM: Color = Color::from_rgb8(142, 162, 196);
const STEEL_SPEC: Color = Color::from_rgb8(226, 238, 255);

// Gold inlay for the ornament. Against blued steel this is what carries the
// ostentation — filigree in the same colour as its ground reads as noise.
const INLAY_DEEP: Color = Color::from_rgb8(84, 60, 20);
const INLAY_MID: Color = Color::from_rgb8(172, 133, 58);
const INLAY_LIT: Color = Color::from_rgb8(240, 210, 140);

// The set stones, tied to the circuitry's cyan so the two ornaments belong to
// the same object.
const GEM_DEEP: Color = Color::from_rgb8(10, 44, 74);
const GEM_BODY: Color = Color::from_rgb8(28, 104, 156);
const GEM_LIT: Color = Color::from_rgb8(120, 208, 240);
const GEM_SPEC: Color = Color::from_rgb8(232, 252, 255);

// A second stone, green, so the vine bears fruit of two kinds rather than a row
// of identical beads. Forest against the sapphire.
const LEAF_DEEP: Color = Color::from_rgb8(14, 52, 30);
const LEAF_BODY: Color = Color::from_rgb8(38, 118, 72);
const LEAF_LIT: Color = Color::from_rgb8(138, 214, 154);

const INNER_SHADOW: Color = Color::from_rgba8(0, 0, 0, 190);

/// A linear gradient between two points, in colour stops.
fn ramp(from: Point, to: Point, stops: &[(f32, Color)]) -> Brush {
    let stops: Vec<ColorStop> = stops
        .iter()
        .map(|(offset, color)| ColorStop {
            offset: *offset,
            color: (*color).into(),
        })
        .collect();
    Brush::Gradient(Gradient::new_linear(from, to).with_stops(stops.as_slice()))
}

/// The frame, painted behind everything else.
///
/// Paints nothing at all when the aesthetic is [`Aesthetic::Flat`], so the flat
/// interface pays only an empty closure for the switch existing.
///
/// The tick is the sky's minute clock: the sun rides the top rail with the
/// day, and a frame that never repaints would leave it stuck at breakfast.
pub fn forged_frame(aesthetic: RwSignal<Aesthetic>, tick: RwSignal<u64>) -> impl IntoView {
    canvas(move |cx, size| {
        if aesthetic.get() != Aesthetic::Forged {
            return;
        }
        tick.get();
        let (w, h) = (size.width, size.height);
        if w < 80.0 || h < 80.0 {
            return;
        }

        draw_moulding(cx, w, h);

        // The sun, riding the top rail with the day. Painted before any
        // ornament so the vines, the corner stones, and the wordmark all crop
        // it — it is part of the frieze, not pasted over it.
        crate::celestial::draw_frame_sun(cx, w);

        // The frieze centre, as a fraction through the frame: past the fillet
        // and ovolo, halfway into the frieze itself.
        const FRIEZE_MID: f64 = 0.15 + 0.25 + 0.35 / 2.0;
        let top_mid = HEADER_HEIGHT as f64 * FRIEZE_MID;
        let side_mid = FRAME_INSET as f64 * FRIEZE_MID;
        let side_band = FRAME_INSET as f64 * 0.35;
        let pad = FRAME_INSET as f64 * 1.6;

        // Ornament rides its own member on every rail, stopping clear of the
        // corners so the volutes there have room to spring — except the bottom
        // rail, which is the fisherman's stage. Two ornaments on one member is
        // one too many, and he is the one anybody came to see.
        draw_vine(
            cx,
            Point::new(side_mid, top_mid + pad),
            Point::new(side_mid, h - side_mid - pad),
            side_band,
            1,
        );
        draw_vine(
            cx,
            Point::new(w - side_mid, top_mid + pad),
            Point::new(w - side_mid, h - side_mid - pad),
            side_band,
            1,
        );

        for corner in 0..4 {
            draw_corner(cx, w, h, corner);
        }
        draw_opening_shadow(cx, w, h);
        // The top rail carries the wordmark on its own frieze rather than a
        // plate of its own, so the header is part of the frame instead of a box
        // floating above it.
        draw_header(cx, w, top_mid);
    })
    .style(|s| {
        s.absolute()
            .width_full()
            .height_full()
            .pointer_events_none()
    })
}

/// A logarithmic spiral as cubic beziers.
///
/// `r = r0 * e^(b*theta)`. Classical volute constructions (Goldmann, Vignola)
/// build the curve from circular quarter-arcs, which is what the originals were
/// drawn with — but each quadrant joint is a curvature discontinuity, and at
/// the sizes here a rasteriser makes those visible as kinks. A true spiral is
/// C2-continuous throughout, so it stays smooth when it is only a few pixels
/// across.
///
/// Handle lengths come from matching both position and tangent at each end:
///
/// ```text
/// lambda_0 = (4/3) * tan(dtheta/4) / (1 + b*tan(dtheta/4))
/// lambda_1 = (4/3) * tan(dtheta/4) / (1 - b*tan(dtheta/4))
/// ```
///
/// At `b = 0` these collapse to `(4/3)*tan(dtheta/4)`, and at a quarter turn
/// that is 0.5523 — the familiar circular constant, which is the check that the
/// general form is right.
fn log_spiral(centre: Point, r0: f64, b: f64, theta0: f64, sweep: f64, segments: usize) -> BezPath {
    let mut path = BezPath::new();
    let at = |theta: f64| -> Point {
        let r = r0 * (b * (theta - theta0)).exp();
        Point::new(centre.x + r * theta.cos(), centre.y + r * theta.sin())
    };
    // Tangent of r = r0*e^(b*theta) in cartesian terms.
    let tangent = |theta: f64| -> (f64, f64) {
        let r = r0 * (b * (theta - theta0)).exp();
        (
            r * (b * theta.cos() - theta.sin()),
            r * (b * theta.sin() + theta.cos()),
        )
    };

    let dtheta = sweep / segments as f64;
    let k = (dtheta / 4.0).tan();
    let l0 = (4.0 / 3.0) * k / (1.0 + b * k);
    let l1 = (4.0 / 3.0) * k / (1.0 - b * k);

    path.move_to(at(theta0));
    for i in 0..segments {
        let a0 = theta0 + dtheta * i as f64;
        let a1 = a0 + dtheta;
        let (t0x, t0y) = tangent(a0);
        let (t1x, t1y) = tangent(a1);
        let p0 = at(a0);
        let p1 = at(a1);
        path.curve_to(
            Point::new(p0.x + l0 * t0x, p0.y + l0 * t0y),
            Point::new(p1.x - l1 * t1x, p1.y - l1 * t1y),
            p1,
        );
    }
    path
}

/// A spiral that *starts* at `from` and curls away, its eye `r` further along
/// `dir`.
///
/// The distinction is the whole of two separate bugs. [`log_spiral`] takes the
/// **eye** of the spiral, and both callers that wanted something to spring
/// from a particular point passed that point as the eye — so the curve was
/// laid down a full radius away from where it was meant to attach. The corner
/// volutes were centred on the corner stone and then buried under it; the
/// vine's tendrils simply floated clear of the stem. Same mistake, two
/// disguises, neither visible in the code.
///
/// Written once, so there is one place to get it right.
fn springing_spiral(from: Point, dir: (f64, f64), r: f64, decay: f64, sweep: f64) -> BezPath {
    let centre = Point::new(from.x + dir.0 * r, from.y + dir.1 * r);
    log_spiral(
        centre,
        r,
        // The decay has to oppose the sweep or the spiral opens out instead of
        // closing: `r = r0 * e^(b * dtheta)` needs `b * sweep` negative.
        -decay * sweep.signum(),
        // Start on the side of the eye that faces `from`.
        (-dir.1).atan2(-dir.0),
        sweep,
        3, // four segments per turn is the documented optimum
    )
}

/// Draw a path three times — dark bed, body, lit edge — so a stroke reads as
/// metal set *into* a surface rather than a line laid on top of it.
fn inlay(cx: &mut floem::context::PaintCx, path: &BezPath, width: f64) {
    cx.stroke(path, INLAY_DEEP, &Stroke::new(width));
    cx.stroke(path, INLAY_MID, &Stroke::new(width * 0.55));
    cx.stroke(path, INLAY_LIT.with_alpha(0.85), &Stroke::new(width * 0.22));
}

/// Which side of the frame a band belongs to.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    Top,
    Right,
    Bottom,
    Left,
}

/// How much of the full light each side receives.
///
/// This is most of what makes a frame read as a solid object rather than a
/// coloured outline. Under a single top-left light the four rails cannot be the
/// same brightness: the top is lit square on, the left glances, the right and
/// bottom are turned away. A frame drawn at uniform brightness reads as a
/// sticker however well each individual member is modelled.
fn side_light(side: Side) -> f64 {
    // Compressed from an earlier 1.0/0.82/0.46/0.34. That range blew out the
    // top rail and sank the bottom one until its ornament floated on nothing.
    // Adjacent faces of one material do not differ threefold.
    match side {
        Side::Top => 1.0,
        Side::Left => 0.86,
        Side::Right => 0.66,
        Side::Bottom => 0.55,
    }
}

/// Scale a colour toward black, in the same proportion across all channels.
fn shade(c: Color, k: f64) -> Color {
    let [r, g, b, a] = c.components;
    Color::new([r * k as f32, g * k as f32, b * k as f32, a])
}

/// One member of one rail, as a mitred quad.
///
/// `t0`/`t1` are fractions through the frame's thickness. Insetting x by the
/// side depth and y by the top depth at the same fraction puts the corner joins
/// on the diagonal automatically, so the mitres come out right even though the
/// top rail is deeper than the others.
fn member_quad(w: f64, h: f64, side: Side, t0: f64, t1: f64) -> BezPath {
    let (top, sidew, bot) = (HEADER_HEIGHT as f64, FRAME_INSET as f64, FRAME_INSET as f64);
    let (x0a, x1a) = (t0 * sidew, t1 * sidew);
    let (y0t, y1t) = (t0 * top, t1 * top);
    let (y0b, y1b) = (h - t0 * bot, h - t1 * bot);

    let pts = match side {
        Side::Top => [
            Point::new(x0a, y0t),
            Point::new(w - x0a, y0t),
            Point::new(w - x1a, y1t),
            Point::new(x1a, y1t),
        ],
        Side::Bottom => [
            Point::new(x0a, y0b),
            Point::new(w - x0a, y0b),
            Point::new(w - x1a, y1b),
            Point::new(x1a, y1b),
        ],
        Side::Left => [
            Point::new(x0a, y0t),
            Point::new(x0a, y0b),
            Point::new(x1a, y1b),
            Point::new(x1a, y1t),
        ],
        Side::Right => [
            Point::new(w - x0a, y0t),
            Point::new(w - x0a, y0b),
            Point::new(w - x1a, y1b),
            Point::new(w - x1a, y1t),
        ],
    };

    let mut path = BezPath::new();
    path.move_to(pts[0]);
    for p in &pts[1..] {
        path.line_to(*p);
    }
    path.close_path();
    path
}

/// The axis a member's gradient runs along: across its width, outer to inner.
///
/// This is the fix for the previous frame reading flat. Its gradients were
/// defined once in canvas space and stroked around the whole perimeter, so
/// along the top rail the colour varied with *x* — bright on the left, dark on
/// the right — instead of rolling from the outer edge to the inner one. A
/// moulding is shaded across its profile; getting the axis wrong makes five
/// carefully modelled members collapse into one flat band.
fn member_axis(w: f64, h: f64, side: Side, t0: f64, t1: f64) -> (Point, Point) {
    let top = HEADER_HEIGHT as f64;
    let sidew = FRAME_INSET as f64;
    match side {
        Side::Top => (Point::new(0.0, t0 * top), Point::new(0.0, t1 * top)),
        Side::Bottom => (
            Point::new(0.0, h - t0 * sidew),
            Point::new(0.0, h - t1 * sidew),
        ),
        Side::Left => (Point::new(t0 * sidew, 0.0), Point::new(t1 * sidew, 0.0)),
        Side::Right => (
            Point::new(w - t0 * sidew, 0.0),
            Point::new(w - t1 * sidew, 0.0),
        ),
    }
}

/// The moulding: five members, only one of them decorated.
///
/// The Rule of Alternation — a decorated profile is always flanked by plain
/// ones. Widths are the classical shares of the frame: fillet 15, ovolo 25,
/// frieze 35, scotia 15, bead 10.
///
/// Drawn as four mitred rails rather than concentric rounded rectangles so each
/// member can be shaded across its own width, which is what gives the frame
/// relief instead of colour.
fn draw_moulding(cx: &mut floem::context::PaintCx, w: f64, h: f64) {
    const MEMBERS: [f64; 5] = [0.15, 0.25, 0.35, 0.15, 0.10];

    for side in [Side::Bottom, Side::Right, Side::Left, Side::Top] {
        let k = side_light(side);
        let mut t = 0.0;
        for (i, share) in MEMBERS.iter().enumerate() {
            let (t0, t1) = (t, t + share);
            let quad = member_quad(w, h, side, t0, t1);
            let (a, b) = member_axis(w, h, side, t0, t1);

            // Each profile has its own luminance signature. A convex ovolo is
            // lit near its crown and falls away; a concave scotia is the
            // inverse, dark where the hollow traps shadow and lifting again
            // from bounce at the far side.
            let stops: Vec<(f32, Color)> = match i {
                0 => vec![(0.0, shade(STEEL_DEEP, k)), (1.0, shade(STEEL_VOID, k))],
                // The ovolo starts at its lit *face*, not at the specular.
                // Opening on STEEL_SPEC spread near-white across a quarter of
                // the rail, which is the broad-soft-gradient signature of
                // plastic — the exact failure the tight-highlight rule exists
                // to avoid. The specular is a separate line below.
                1 => vec![
                    (0.0, shade(STEEL_FACE, k)),
                    (0.35, shade(STEEL_BODY, k)),
                    (1.0, shade(STEEL_DEEP, k)),
                ],
                2 => vec![
                    (0.0, shade(STEEL_VOID, k)),
                    (0.5, shade(Color::from_rgb8(16, 20, 28), k)),
                    (1.0, shade(STEEL_VOID, k)),
                ],
                3 => vec![
                    (0.0, shade(STEEL_VOID, k)),
                    (0.55, shade(STEEL_DEEP, k)),
                    (1.0, shade(STEEL_BODY, k)),
                ],
                _ => vec![
                    (0.0, shade(INLAY_LIT, k)),
                    (0.4, shade(INLAY_MID, k)),
                    (1.0, shade(INLAY_DEEP, k)),
                ],
            };
            cx.fill(&quad, &ramp(a, b, &stops), 0.0);
            t = t1;
        }

        // A hard specular along the outermost edge of the lit rails only. The
        // narrowness is the point: a wide soft highlight reads as plastic, a
        // tight bright line reads as polished metal.
        // Narrow by construction: a hairline at the fillet/ovolo joint, which
        // is where a real moulding catches the light.
        let hi = member_quad(w, h, side, 0.15, 0.175);
        cx.fill(&hi, shade(STEEL_SPEC, k * 0.95), 0.0);
        let rim = member_quad(w, h, side, 0.175, 0.21);
        cx.fill(&rim, shade(STEEL_RIM, k * 0.8), 0.0);
    }
}

/// A lanceolate leaf, as two mirrored curves from base to tip.
///
/// Leaves are where vector earns its keep against a texture: a leaf is two
/// bezier arcs and a midrib, and it stays crisp at any size. The bulge factors
/// put the widest point around a third of the length, which is what separates
/// a leaf from a lens.
fn leaf(base: Point, dir: (f64, f64), len: f64, width: f64) -> BezPath {
    let (dx, dy) = dir;
    let (nx, ny) = (-dy, dx);
    let tip = Point::new(base.x + dx * len, base.y + dy * len);
    let at = |along: f64, across: f64| {
        Point::new(
            base.x + dx * len * along + nx * width * across,
            base.y + dy * len * along + ny * width * across,
        )
    };

    let mut path = BezPath::new();
    path.move_to(base);
    path.curve_to(at(0.20, 0.62), at(0.68, 0.42), tip);
    path.curve_to(at(0.68, -0.42), at(0.20, -0.62), base);
    path.close_path();
    path
}

/// A leaf on the vine, as the numbers it is drawn from.
#[derive(Clone, Copy)]
struct Leaf {
    base: Point,
    dir: (f64, f64),
    len: f64,
    width: f64,
}

impl Leaf {
    fn outline(&self) -> BezPath {
        leaf(self.base, self.dir, self.len, self.width)
    }

    /// The lit inner fill, slightly inside the outline so a rim of the dark
    /// bed shows all the way round.
    fn inner(&self) -> BezPath {
        leaf(self.base, self.dir, self.len * 0.88, self.width * 0.83)
    }

    fn rib(&self) -> BezPath {
        let mut path = BezPath::new();
        path.move_to(self.base);
        path.line_to(Point::new(
            self.base.x + self.dir.0 * self.len * 0.88,
            self.base.y + self.dir.1 * self.len * 0.88,
        ));
        path
    }
}

/// A tendril: a short spiral springing off the stem.
#[derive(Clone, Copy)]
struct Tendril {
    origin: Point,
    dir: (f64, f64),
    r: f64,
    hand: f64,
}

impl Tendril {
    fn path(&self) -> BezPath {
        springing_spiral(
            self.origin,
            self.dir,
            self.r,
            VINE_TENDRIL_DECAY,
            std::f64::consts::TAU * 0.55 * self.hand,
        )
    }
}

/// A stone set on the vine, where the stem returns to its axis.
#[derive(Clone, Copy)]
struct Berry {
    at: Point,
    r: f64,
    cool: bool,
}

/// A rinceau, as geometry, before any of it is painted.
///
/// Separated from the drawing for the same reason as the corner volutes:
/// everything that can be *wrong* about a vine — whether the stem is smooth,
/// whether the leaves are attached to it, whether the run fits the member it
/// rides — is a question about numbers, and none of it needs a screenshot.
struct Rinceau {
    stem: Vec<CubicBez>,
    stem_width: f64,
    leaves: Vec<Leaf>,
    tendrils: Vec<Tendril>,
    berries: Vec<Berry>,
}

impl Rinceau {
    /// The stem as one path. One path rather than one stroke per repeat: a
    /// stroke that stops and restarts shows where it did.
    fn stem_path(&self) -> BezPath {
        let mut path = BezPath::new();
        let Some(first) = self.stem.first() else {
            return path;
        };
        path.move_to(first.p0);
        for arc in &self.stem {
            path.curve_to(arc.p1, arc.p2, arc.p3);
        }
        path
    }
}

/// A small stone on the vine.
///
/// Below the radius where facets resolve, so drawn as a cabochon — a radial
/// gradient with one off-centre glint, which is what a small polished stone
/// actually looks like.
fn draw_berry(cx: &mut floem::context::PaintCx, c: Point, r: f64, cool: bool) {
    let (deep, body, lit) = if cool {
        (GEM_DEEP, GEM_BODY, GEM_LIT)
    } else {
        (LEAF_DEEP, LEAF_BODY, LEAF_LIT)
    };
    cx.fill(&Circle::new(c, r + 1.1), INLAY_DEEP, 0.0);
    cx.fill(&Circle::new(c, r + 0.5), INLAY_MID, 0.0);
    cabochon(cx, c, r, deep, body, lit);
}

/// A small polished stone: three discs, each offset toward the light.
///
/// **Not a radial gradient**, and that is not a style preference. floem's vger
/// backend drops radial and sweep gradients on the floor —
/// `GradientKind::Radial { .. } => return None` in `brush_to_paint`, and `fill`
/// returns immediately when the brush resolves to `None`. So a radial fill
/// draws *nothing at all*, silently. Every berry on the vine has been an empty
/// gold ring since the day it was written, and nothing said so because there is
/// no error path: the shape is simply skipped.
///
/// Offsetting concentric discs toward the highlight gives the same read — a
/// bright core falling off to a dark rim — out of primitives that are actually
/// rasterised.
fn cabochon(
    cx: &mut floem::context::PaintCx,
    c: Point,
    r: f64,
    deep: Color,
    body: Color,
    lit: Color,
) {
    let toward_light = |t: f64| Point::new(c.x - r * t, c.y - r * t);
    cx.fill(&Circle::new(c, r), deep, 0.0);
    cx.fill(&Circle::new(toward_light(0.16), r * 0.82), body, 0.0);
    cx.fill(&Circle::new(toward_light(0.34), r * 0.42), lit, 0.0);
}

/// A foliate scroll — a rinceau — running the length of a frieze.
///
/// Replaces a two-strand guilloche. A woven band is correct and classical, but
/// it is *machine* ornament: one figure repeated without variation. Nature
/// imitated in metal wants a stem that grows — leaves alternating either side,
/// tendrils curling off, fruit set where the stem crosses its own axis.
///
/// The repeat is the classical running scroll: one unit is 1.5 times the band
/// height. The stem is drawn as a single continuous serpentine through every
/// repeat rather than as separate units, so the run reads as one growth instead
/// of a row of stamps.
fn draw_vine(cx: &mut floem::context::PaintCx, from: Point, to: Point, band: f64, seed: usize) {
    let Some(vine) = rinceau(from, to, band, seed) else {
        return;
    };

    inlay(cx, &vine.stem_path(), vine.stem_width);

    for l in &vine.leaves {
        cx.fill(&l.outline(), INLAY_DEEP, 0.0);
        cx.fill(&l.inner(), INLAY_MID, 0.0);
        cx.stroke(&l.outline(), INLAY_LIT.with_alpha(0.75), &Stroke::new(0.8));
        cx.stroke(&l.rib(), INLAY_LIT.with_alpha(0.6), &Stroke::new(0.7));
    }

    for t in &vine.tendrils {
        let path = t.path();
        cx.stroke(&path, INLAY_DEEP, &Stroke::new(VINE_TENDRIL_INLAY));
        cx.stroke(&path, INLAY_MID, &Stroke::new(VINE_TENDRIL_INLAY * 0.5));
    }

    for b in &vine.berries {
        draw_berry(cx, b.at, b.r, b.cool);
    }
}

/// The geometry of one run of vine.
///
/// `None` when the run is too short to carry a repeat at all, which is what
/// keeps a narrow window from drawing a single mangled unit.
fn rinceau(from: Point, to: Point, band: f64, seed: usize) -> Option<Rinceau> {
    let dx = to.x - from.x;
    let dy = to.y - from.y;
    let len = (dx * dx + dy * dy).sqrt();
    if len < 60.0 {
        return None;
    }
    let (ux, uy) = (dx / len, dy / len);
    let (nx, ny) = (-uy, ux);

    // Sized from the half-width of the member, because the ornament is
    // measured either side of the run's centre line and that is the room it
    // has in each direction.
    let half = band / 2.0;
    let count = (len / (band * VINE_REPEAT)).floor().max(1.0) as usize;
    let repeat = len / count as f64;
    let at = |d: f64, off: f64| Point::new(from.x + ux * d + nx * off, from.y + uy * d + ny * off);

    // One repeat is a *half* wave, both control points on the same side, and
    // the side alternates. That is what makes the joins smooth: the arc
    // arrives at a node on the tangent the next one leaves on. Building each
    // repeat as a complete S instead — up then down, within the one segment —
    // reverses the tangent at every node, which is a cusp, and turns a vine
    // into a row of separate wiggles.
    //
    // A cubic with both controls offset by `c` crests at `0.75 * c`.
    let control = half * VINE_AMPLITUDE / 0.75;

    let mut stem = Vec::with_capacity(count);
    let mut leaves = Vec::with_capacity(count);
    let mut tendrils = Vec::with_capacity(count);
    let mut berries = Vec::new();

    for i in 0..count {
        let d0 = i as f64 * repeat;
        let side = if (i + seed).is_multiple_of(2) {
            1.0
        } else {
            -1.0
        };

        let arc = CubicBez::new(
            at(d0, 0.0),
            at(d0 + repeat / 3.0, control * side),
            at(d0 + repeat * 2.0 / 3.0, control * side),
            at(d0 + repeat, 0.0),
        );

        // Leaf at the crest, raked forward and standing away from the axis.
        // Base *and* orientation are read off the stem rather than off the
        // run's axis: an offset that merely resembles the curve is how the
        // leaves came to sit clear of the thing they grow from.
        let (base, along) = frame_on(&arc, 0.5);
        let (lx, ly) = (
            along.0 * VINE_LEAF_RAKE - along.1 * side,
            along.1 * VINE_LEAF_RAKE + along.0 * side,
        );
        let lm = lx.hypot(ly);
        leaves.push(Leaf {
            base,
            dir: (lx / lm, ly / lm),
            len: half * VINE_LEAF_LEN,
            width: half * VINE_LEAF_WIDTH,
        });

        // Tendril springing from the stem further along, curling into the
        // hollow of the bend — the side the leaf is not on.
        let (origin, along) = frame_on(&arc, 0.86);
        tendrils.push(Tendril {
            origin,
            dir: (along.1 * side, -along.0 * side),
            r: half * VINE_TENDRIL,
            hand: side,
        });

        // Fruit where the stem returns to its axis.
        if i.is_multiple_of(2) {
            berries.push(Berry {
                at: arc.p3,
                r: half * VINE_BERRY,
                cool: (i / 2 + seed).is_multiple_of(2),
            });
        }

        stem.push(arc);
    }

    Some(Rinceau {
        stem,
        stem_width: half * VINE_STEM,
        leaves,
        tendrils,
        berries,
    })
}

/// A point on the stem and the unit tangent there.
///
/// Everything hung on the vine is placed in this frame, which is what makes it
/// attached by construction rather than by a coincidence of arithmetic that
/// has to be maintained by hand.
fn frame_on(arc: &CubicBez, t: f64) -> (Point, (f64, f64)) {
    let d = arc.deriv().eval(t);
    let m = d.x.hypot(d.y);
    (arc.eval(t), (d.x / m, d.y / m))
}

/// A corner: the frieze turns, and the turn is where the ornament roots.
///
/// Jones's eleventh proposition — every ornament should trace to its branch and
/// root — is the rule the first attempt broke. Volutes floating mid-band had
/// nothing to grow from. Here they spring from the corner, which is a real
/// structural event.
fn draw_corner(cx: &mut floem::context::PaintCx, w: f64, h: f64, corner: usize) {
    let (eye, sx, sy) = corner_anchor(w, h, corner);

    // Volutes first, stone second: the bezel then covers where they spring
    // from, which is what roots them to it rather than leaving them abutting.
    for volute in corner_volutes(eye, sx, sy) {
        inlay(cx, &volute, VOLUTE_INLAY);
    }

    draw_gem(cx, eye, CORNER_GEM_R, sx, sy);
}

/// Where a corner's ornament roots, and which way the two rails run from it.
///
/// `sx`/`sy` point *into* the frame along the horizontal and vertical rails, so
/// one set of formulae serves all four corners.
fn corner_anchor(w: f64, h: f64, corner: usize) -> (Point, f64, f64) {
    const FRIEZE_MID: f64 = 0.15 + 0.25 + 0.35 / 2.0;
    let side_mid = FRAME_INSET as f64 * FRIEZE_MID;
    let top_mid = HEADER_HEIGHT as f64 * FRIEZE_MID;
    match corner {
        0 => (Point::new(side_mid, top_mid), 1.0, 1.0),
        1 => (Point::new(w - side_mid, top_mid), -1.0, 1.0),
        2 => (Point::new(side_mid, h - side_mid), 1.0, -1.0),
        _ => (Point::new(w - side_mid, h - side_mid), -1.0, -1.0),
    }
}

/// The pair of volutes at one corner, as paths.
///
/// Separate from the painting because this is geometry, and geometry is the
/// part of ornament that can be checked without looking at it. It needed to be:
/// the first version was concentric with the corner stone and wound *inwards*
/// beneath it, so almost all of it was painted and then covered. That is a
/// three-line arithmetic result and it had gone unexplained for a session,
/// because nothing here was in a shape anything could ask a question of.
///
/// One volute springs along each rail that meets at the corner. Each spiral's
/// own eye sits out along its rail, and the curve begins on the *far* side of
/// that eye — on the stone's bezel — and winds in. So its closest approach to
/// the corner is where it springs from, and the whole of the rest of it grows
/// away into the bare stretch of frieze before the vine starts.
///
/// The two counter-rotate: mirroring across the corner's diagonal reverses
/// handedness, which is why the signs are `sx * sy` and its negation, and why
/// they invert again on the far corners.
fn corner_volutes(eye: Point, sx: f64, sy: f64) -> [BezPath; 2] {
    [(sx, 0.0, sx * sy), (0.0, sy, -sx * sy)].map(|(dx, dy, hand)| {
        springing_spiral(
            // From the stone's rim, so the bezel covers the join.
            Point::new(eye.x + dx * CORNER_GEM_OUTER, eye.y + dy * CORNER_GEM_OUTER),
            (dx, dy),
            VOLUTE_R,
            VOLUTE_DECAY,
            std::f64::consts::TAU * 0.75 * hand,
        )
    })
}

/// The shadow the frame casts into its own opening.
///
/// Without it the shell looks pasted onto the moulding rather than sitting
/// down inside it — the frame and the work read as two flat layers instead of
/// one object with depth.
fn draw_opening_shadow(cx: &mut floem::context::PaintCx, w: f64, h: f64) {
    let inset = FRAME_INSET as f64;
    let top = HEADER_HEIGHT as f64;
    cx.stroke(
        &Rect::new(inset, top, w - inset, h - inset).to_rounded_rect(INNER_RADIUS),
        INNER_SHADOW,
        &Stroke::new(5.0),
    );
    cx.stroke(
        &Rect::new(inset + 2.5, top + 2.5, w - inset - 2.5, h - inset - 2.5)
            .to_rounded_rect(INNER_RADIUS),
        STEEL_VOID.with_alpha(0.5),
        &Stroke::new(2.0),
    );
}

/// A set stone, with a level of detail that follows its size.
///
/// Faceting stops being legible below a few pixels — the facets fall under the
/// resolvable limit and the whole thing collapses into a coloured dot with
/// noise on it. The documented ladder is a full brilliant at large sizes, a
/// 17-facet single cut in the middle, and below that no faceting at all: a flat
/// circle with a two-stop radial gradient, which is honest about what can
/// actually be seen. Sizing a stone at 5.5px and drawing eight facets into it,
/// as the first attempt did, is the case this exists to prevent.
fn draw_gem(cx: &mut floem::context::PaintCx, c: Point, r: f64, sx: f64, sy: f64) {
    // Bezel, always. Opaque, and drawn over whatever came before it — see
    // `CORNER_GEM_OUTER`.
    cx.fill(&Circle::new(c, r + GEM_BEZEL), INLAY_DEEP, 0.0);
    cx.fill(&Circle::new(c, r + GEM_BEZEL * 0.5), INLAY_MID, 0.0);

    if r < 4.0 {
        // Too small to face — a cabochon is what it actually looks like. Drawn
        // as offset discs rather than a radial gradient, which vger discards;
        // see `cabochon`.
        cabochon(cx, c, r, GEM_DEEP, GEM_BODY, GEM_LIT);
        return;
    }

    // Single cut: an octagonal table with eight crown facets around it.
    let facets = 8usize;
    let pt = |i: usize, radius: f64| -> Point {
        let a = (i as f64 / facets as f64) * std::f64::consts::TAU - std::f64::consts::FRAC_PI_8;
        Point::new(c.x + radius * a.cos(), c.y + radius * a.sin())
    };

    for i in 0..facets {
        let mut facet = BezPath::new();
        facet.move_to(pt(i, r * 0.42));
        facet.line_to(pt(i, r));
        facet.line_to(pt((i + 1) % facets, r));
        facet.line_to(pt((i + 1) % facets, r * 0.42));
        facet.close_path();

        // Brightness from how far the facet faces the light at the top-left,
        // flipped with the corner so every stone is lit from the same place.
        let a = (i as f64 + 0.5) / facets as f64 * std::f64::consts::TAU;
        let facing = (a.cos() * sx + a.sin() * sy) * -0.5 + 0.5;
        let colour = if facing > 0.70 {
            GEM_LIT
        } else if facing > 0.38 {
            GEM_BODY
        } else {
            GEM_DEEP
        };
        cx.fill(&facet, colour, 0.0);
    }

    // Table, then the specular that sells it as cut rather than moulded.
    let mut table = BezPath::new();
    table.move_to(pt(0, r * 0.42));
    for i in 1..facets {
        table.line_to(pt(i, r * 0.42));
    }
    table.close_path();
    cx.fill(&table, GEM_BODY, 0.0);
    cx.fill(
        &Circle::new(
            Point::new(c.x - r * 0.30 * sx, c.y - r * 0.32 * sy),
            r * 0.15,
        ),
        GEM_SPEC,
        0.0,
    );
}

/// The wordmark, riding the top rail's own frieze.
///
/// Previously this drew a rounded plate of its own, which read as a separate
/// box floating above the frame rather than as part of it. The top rail is
/// already deeper than the others precisely so it can carry the word; giving it
/// a second surface to sit on was the mistake.
fn draw_header(cx: &mut floem::context::PaintCx, w: f64, mid_y: f64) {
    let text_w = draw_wordmark(cx, w, mid_y);

    // Guilloche either side of the word, on the same continuous principle as
    // the other rails, withdrawing entirely rather than crowding it.
    let band = HEADER_HEIGHT as f64 * 0.35;
    let gap = text_w / 2.0 + 30.0;
    let span = (w / 2.0 - gap - FRAME_INSET as f64 * 2.0).min(260.0);
    if span >= 60.0 {
        for dir in [-1.0_f64, 1.0] {
            let start = Point::new(w / 2.0 + dir * gap, mid_y);
            let end = Point::new(start.x + dir * span, mid_y);
            draw_vine(cx, start, end, band, if dir > 0.0 { 0 } else { 1 });
        }
    }
}

/// The wordmark, engraved and tracked out. Returns its drawn width.
///
/// Each glyph is laid out and positioned separately: letterspacing is not
/// exposed through the text attributes, and at this size a wide-tracked serif
/// carries far more presence than a tight one. The word is drawn three times —
/// a dark bed, a mid body, a light top edge — the same recipe as the inlay, so
/// the lettering belongs to the same object as the filigree.
fn draw_wordmark(cx: &mut floem::context::PaintCx, w: f64, mid_y: f64) -> f64 {
    let glyph = |ch: char, size: f32, colour: Color| {
        let attrs = Attrs::new()
            .family(WORDMARK_FAMILY)
            .font_size(size)
            .weight(FontWeight::BOLD)
            .color(colour);
        TextLayout::new_with_text(&ch.to_string(), AttrsList::new(attrs), None)
    };

    let widths: Vec<f64> = WORDMARK
        .chars()
        .map(|c| glyph(c, WORDMARK_SIZE, INLAY_MID).size().width)
        .collect();
    let total: f64 =
        widths.iter().sum::<f64>() + WORDMARK_TRACKING * (WORDMARK.chars().count() - 1) as f64;

    // Engraved, not raised: the shadow sits on the *upper-left* inner wall,
    // which is the one facing away from a top-left light, and the highlight on
    // the lower-right. The previous order — dark below, light above — is the
    // recipe for relief, so the wordmark was standing proud rather than cut in.
    for (colour, dx, dy) in [
        (STEEL_VOID, -0.9, -0.9),
        (INLAY_MID, 0.0, 0.0),
        (INLAY_LIT, 0.7, 0.7),
    ] {
        let mut x = (w - total) / 2.0;
        for (i, ch) in WORDMARK.chars().enumerate() {
            let layout = glyph(ch, WORDMARK_SIZE, colour);
            let size = layout.size();
            layout.draw(cx, Point::new(x + dx, mid_y - size.height / 2.0 + dy));
            x += widths[i] + WORDMARK_TRACKING;
        }
    }
    total
}

// --- Circuitry behind the editor -------------------------------------------

/// The trace palette, quietest first. Which one is used depends on what the
/// language server is currently reporting, so the backdrop is a readout rather
/// than a decoration.
const TRACE_CLEAN: Color = Color::from_rgb8(0, 168, 190);
const TRACE_WARN: Color = Color::from_rgb8(214, 158, 74);
const TRACE_ERROR: Color = Color::from_rgb8(214, 84, 100);
/// The mosaic the traces run over.
const TILE: Color = Color::from_rgb8(24, 28, 38);

/// How many traces the backdrop draws.
const TRACE_COUNT: usize = 7;

/// What the circuitry is currently reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CircuitReading {
    /// The worst severity currently reported anywhere.
    pub worst: Option<Severity>,
    /// How many diagnostics there are, capped at the number of traces — the
    /// backdrop cannot show more than it has traces to light.
    pub lit: usize,
}

/// Reduce the diagnostics to what the backdrop can actually show.
///
/// Separated from the drawing so the mapping can be tested: everything else in
/// this module is paint, but this is the part that carries meaning, and it
/// being wrong would mean a calm-looking backdrop over a broken file.
pub fn circuit_reading(by_file: &[(String, Vec<ProblemRow>)]) -> CircuitReading {
    let mut worst: Option<Severity> = None;
    let mut count = 0usize;

    for (_, rows) in by_file {
        for row in rows {
            count += 1;
            worst = Some(match worst {
                // Error beats Warning beats everything quieter. Ordering is
                // spelled out rather than derived, because `Severity`'s
                // declaration order is an implementation detail of the LSP
                // mapping and should not silently define severity here.
                Some(Severity::Error) => Severity::Error,
                _ if row.severity == Severity::Error => Severity::Error,
                Some(Severity::Warning) => Severity::Warning,
                _ if row.severity == Severity::Warning => Severity::Warning,
                Some(existing) => existing,
                None => row.severity,
            });
        }
    }

    CircuitReading {
        worst,
        lit: count.min(TRACE_COUNT),
    }
}

fn trace_colour(worst: Option<Severity>) -> Color {
    match worst {
        Some(Severity::Error) => TRACE_ERROR,
        Some(Severity::Warning) => TRACE_WARN,
        _ => TRACE_CLEAN,
    }
}

/// Circuit traces behind the editor, lit by what the language server reports.
///
/// Deliberately dim: this sits under code that has to stay readable, so the
/// brightest thing here is well below the dimmest syntax colour. It reads as
/// texture at a glance and as a signal when you look for it.
pub fn circuit_backdrop(
    aesthetic: RwSignal<Aesthetic>,
    by_file: RwSignal<Vec<(String, Vec<ProblemRow>)>>,
) -> impl IntoView {
    canvas(move |cx, size| {
        if aesthetic.get() != Aesthetic::Forged {
            return;
        }
        let reading = circuit_reading(&by_file.get());
        let colour = trace_colour(reading.worst);
        let (w, h) = (size.width, size.height);
        if w < 1.0 || h < 1.0 {
            return;
        }

        draw_mosaic(cx, w, h);

        // Traces are spaced deterministically from their index rather than
        // randomly, so the backdrop does not crawl as the panel resizes.
        for i in 0..TRACE_COUNT {
            let t = (i as f64 + 1.0) / (TRACE_COUNT as f64 + 1.0);
            let y = h * t;
            let lit = i < reading.lit;
            draw_trace(cx, w, y, h, i, colour, lit);
        }
    })
    // Decoration only: without this the backdrop would sit over the editor for
    // hit-testing and swallow every click meant for the code.
    .style(|s| {
        s.absolute()
            .width_full()
            .height_full()
            .pointer_events_none()
    })
}

/// The tile grid the traces run over.
fn draw_mosaic(cx: &mut floem::context::PaintCx, w: f64, h: f64) {
    const TILE_SIZE: f64 = 26.0;
    let cols = (w / TILE_SIZE).ceil() as i64;
    let rows = (h / TILE_SIZE).ceil() as i64;

    for row in 0..rows {
        for col in 0..cols {
            // A cheap deterministic checker, varied enough to read as a mosaic
            // rather than a grid, and stable across repaints.
            if (row * 7 + col * 3) % 5 != 0 {
                continue;
            }
            let x = col as f64 * TILE_SIZE;
            let y = row as f64 * TILE_SIZE;
            cx.fill(
                &Rect::new(x + 1.0, y + 1.0, x + TILE_SIZE - 1.0, y + TILE_SIZE - 1.0)
                    .to_rounded_rect(2.0),
                TILE.with_alpha(0.55),
                0.0,
            );
        }
    }
}

/// One trace: a stepped path across the panel, with a junction node.
fn draw_trace(
    cx: &mut floem::context::PaintCx,
    w: f64,
    y: f64,
    h: f64,
    index: usize,
    colour: Color,
    lit: bool,
) {
    // Where the trace steps to a different level, varied per index so the set
    // reads as routed rather than as parallel rules.
    let turn = w * (0.28 + 0.09 * (index % 4) as f64);
    let drop = h * 0.06 * if index.is_multiple_of(2) { 1.0 } else { -1.0 };

    let mut path = BezPath::new();
    path.move_to(Point::new(0.0, y));
    path.line_to(Point::new(turn - 14.0, y));
    path.quad_to(
        Point::new(turn, y),
        Point::new(turn + 14.0, (y + drop).clamp(2.0, h - 2.0)),
    );
    path.line_to(Point::new(w, (y + drop).clamp(2.0, h - 2.0)));

    // A wide, faint pass under a narrow brighter one is what reads as glow
    // without a blur filter.
    let (glow, core) = if lit { (0.20, 0.55) } else { (0.07, 0.16) };
    cx.stroke(&path, colour.with_alpha(glow), &Stroke::new(5.0));
    cx.stroke(&path, colour.with_alpha(core), &Stroke::new(1.2));

    let node = Point::new(turn, y);
    cx.fill(&Circle::new(node, 3.0), colour.with_alpha(glow), 0.0);
    cx.fill(&Circle::new(node, 1.6), colour.with_alpha(core), 0.0);
}

/// How far the shell is inset for a given aesthetic.
///
/// A function rather than a constant so the shell's style closure stays a
/// one-liner and the two looks cannot drift apart.
pub fn shell_inset(aesthetic: Aesthetic) -> f32 {
    match aesthetic {
        Aesthetic::Flat => 0.0,
        Aesthetic::Forged => FRAME_INSET,
    }
}

/// How far the shell is inset at the top, which is deeper to seat the wordmark.
pub fn shell_top_inset(aesthetic: Aesthetic) -> f32 {
    match aesthetic {
        Aesthetic::Flat => 0.0,
        Aesthetic::Forged => HEADER_HEIGHT,
    }
}

/// Like the design-token invariants, these compare `const`s: clippy is right
/// that the values are known at compile time, and that is the point — they
/// exist to fail the build if someone retunes the frame and breaks its
/// geometry, not to exercise runtime behaviour.
#[cfg(test)]
#[allow(clippy::assertions_on_constants)]
mod tests {
    use super::*;

    /// The flat interface must not be inset — any padding at all would move
    /// every panel and betray that the ornament exists.
    #[test]
    fn only_the_forged_look_insets_the_shell() {
        assert_eq!(shell_inset(Aesthetic::Flat), 0.0);
        assert!(shell_inset(Aesthetic::Forged) > 0.0);
    }

    /// The opening has to sit inside the frame with room for the bevel, or the
    /// shell would paint over the chamfer and there would be no visible frame.
    fn row(severity: Severity) -> ProblemRow {
        ProblemRow {
            file: "src/main.rs".into(),
            line: 1,
            column: 1,
            severity,
            message: String::new(),
            code: None,
            source: None,
        }
    }

    /// The backdrop is a readout, so the thing worth pinning is that it cannot
    /// look calm over a broken file. Everything else in this module is paint.
    #[test]
    fn an_error_anywhere_outranks_every_quieter_diagnostic() {
        let files = vec![
            (
                "a.rs".to_string(),
                vec![row(Severity::Hint), row(Severity::Warning)],
            ),
            ("b.rs".to_string(), vec![row(Severity::Information)]),
            ("c.rs".to_string(), vec![row(Severity::Error)]),
        ];
        assert_eq!(circuit_reading(&files).worst, Some(Severity::Error));

        // Order must not matter: the error arriving first is the same reading.
        let reversed: Vec<_> = files.into_iter().rev().collect();
        assert_eq!(circuit_reading(&reversed).worst, Some(Severity::Error));
    }

    #[test]
    fn warnings_outrank_hints_but_not_errors() {
        let warn = vec![(
            "a.rs".to_string(),
            vec![row(Severity::Hint), row(Severity::Warning)],
        )];
        assert_eq!(circuit_reading(&warn).worst, Some(Severity::Warning));

        let clean = vec![("a.rs".to_string(), vec![row(Severity::Hint)])];
        assert_eq!(circuit_reading(&clean).worst, Some(Severity::Hint));
    }

    #[test]
    fn a_clean_project_lights_nothing() {
        let reading = circuit_reading(&[]);
        assert_eq!(reading.worst, None);
        assert_eq!(reading.lit, 0);
    }

    /// More diagnostics than traces must not overflow the backdrop — it can
    /// only light what it draws.
    #[test]
    fn the_lit_count_is_capped_at_the_number_of_traces() {
        let many = vec![(
            "a.rs".to_string(),
            (0..500).map(|_| row(Severity::Error)).collect::<Vec<_>>(),
        )];
        assert_eq!(circuit_reading(&many).lit, TRACE_COUNT);
    }

    /// The shell must clear the wordmark plate, or the header would be drawn
    /// under the menu bar and neither would be readable.
    #[test]
    fn the_forged_top_inset_clears_the_header() {
        assert!(
            shell_top_inset(Aesthetic::Forged) >= HEADER_HEIGHT,
            "the shell has to start below the wordmark plate"
        );
        assert_eq!(shell_top_inset(Aesthetic::Flat), 0.0);
        assert!(
            shell_top_inset(Aesthetic::Forged) > shell_inset(Aesthetic::Forged),
            "the top is deeper than the sides — that is what makes room for it"
        );
    }

    /// The five members must account for the whole frame. A short sum leaves a
    /// gap the background shows through; a long one overlaps the opening.
    #[test]
    fn the_moulding_members_sum_to_the_whole_frame() {
        let total: f64 = [0.15, 0.25, 0.35, 0.15, 0.10].iter().sum();
        assert!(
            (total - 1.0).abs() < 1e-9,
            "members sum to {total}, not 1.0"
        );
    }

    /// Points along a path, densely enough that no excursion between two
    /// control points is missed. The curve's own endpoints are included, which
    /// matters: the volute's closest approach to the corner is its first point.
    fn sample(path: &BezPath) -> Vec<Point> {
        use floem::peniko::kurbo::ParamCurve;
        let mut pts = Vec::new();
        for seg in path.segments() {
            for i in 0..=64 {
                pts.push(seg.eval(i as f64 / 64.0));
            }
        }
        pts
    }

    /// A test canvas large enough that the four corners do not overlap.
    const W: f64 = 900.0;
    const H: f64 = 640.0;

    /// The volutes are painted *before* the corner stone, and the stone's bezel
    /// is an opaque fill. So anything a volute draws within that disc is drawn
    /// and then covered.
    ///
    /// This is the defect the first version had, and it was invisible in the
    /// code: the volutes were concentric with the stone and wound *inwards*
    /// from `frame * 0.30` to about `frame * 0.11`, against a bezel reaching
    /// `frame * 0.22 + 1.6`. Five sixths of every volute's sweep was buried, so
    /// the corners read as bare and the ornament looked as though it had never
    /// been drawn.
    ///
    /// The bound allows half the inlay's width, and that is deliberate rather
    /// than slack: the volute is *meant* to spring from the bezel rim, so its
    /// stroke tucks under the setting by half its width and the stone is drawn
    /// over the join. An ornament with a clear gap between it and its root
    /// would fail Jones's eleventh proposition in the other direction. What is
    /// forbidden is the volute's whole painted width lying inside the stone.
    #[test]
    fn a_corner_volute_is_never_covered_by_the_stone_drawn_over_it() {
        let buried = CORNER_GEM_OUTER - VOLUTE_INLAY / 2.0;
        for corner in 0..4 {
            let (eye, sx, sy) = corner_anchor(W, H, corner);
            for volute in corner_volutes(eye, sx, sy) {
                for p in sample(&volute) {
                    let d = (p - eye).hypot();
                    assert!(
                        d >= buried,
                        "corner {corner}: the volute passes {d:.2} from the eye, \
                         under a stone reaching {CORNER_GEM_OUTER:.2} — that stretch \
                         is painted and then covered"
                    );
                }
            }
        }
    }

    /// The Rule of Alternation: the decorated member is flanked by plain ones.
    /// An ornament that swings off its own frieze onto the ovolo or the scotia
    /// breaks the alternation and, at this scale, simply looks like a mistake.
    ///
    /// Measured against the *side* rails' half-band even for the volute riding
    /// the top rail, which is deeper — the pair should match each other, so the
    /// narrower rail sets the size for both. The inlay's own width counts:
    /// paint spreads half of it either side of the path.
    #[test]
    fn a_corner_volute_stays_on_the_frieze_it_rides() {
        const FRIEZE_HALF: f64 = FRAME_INSET as f64 * 0.35 / 2.0;
        let budget = FRIEZE_HALF - VOLUTE_INLAY / 2.0;
        for corner in 0..4 {
            let (eye, sx, sy) = corner_anchor(W, H, corner);
            let volutes = corner_volutes(eye, sx, sy);
            for (i, volute) in volutes.iter().enumerate() {
                for p in sample(volute) {
                    // The first rides the horizontal rail, so what must stay on
                    // the band is its distance across in y; the second mirrors.
                    let across = if i == 0 {
                        (p.y - eye.y).abs()
                    } else {
                        (p.x - eye.x).abs()
                    };
                    assert!(
                        across <= budget,
                        "corner {corner} volute {i}: {across:.2} across the rail, \
                         off a frieze that allows {budget:.2}"
                    );
                }
            }
        }
    }

    /// Jones's eleventh proposition: every ornament traced to its branch and
    /// root. A corner is where the frieze actually turns, and the volutes are
    /// the turn — so each one runs *out along a rail*, into the bare stretch
    /// before the vine begins. Two spirals ringing the stone are decoration
    /// applied to a corner, not ornament growing from one.
    #[test]
    fn the_two_volutes_at_a_corner_spring_along_the_two_rails_that_meet_there() {
        for corner in 0..4 {
            let (eye, sx, sy) = corner_anchor(W, H, corner);
            let [horizontal, vertical] = corner_volutes(eye, sx, sy);

            // Reach along each rail, signed so that positive is into the frame.
            let reach = |path: &BezPath, f: &dyn Fn(Point) -> f64| {
                sample(path).into_iter().map(f).fold(f64::MIN, f64::max)
            };
            let h_reach = reach(&horizontal, &|p: Point| (p.x - eye.x) * sx);
            let v_reach = reach(&vertical, &|p: Point| (p.y - eye.y) * sy);

            assert!(
                h_reach > CORNER_GEM_OUTER,
                "corner {corner}: the horizontal volute reaches {h_reach:.2} along \
                 its rail, no further than the stone it should be growing out of"
            );
            assert!(
                v_reach > CORNER_GEM_OUTER,
                "corner {corner}: the vertical volute reaches {v_reach:.2} along \
                 its rail, no further than the stone it should be growing out of"
            );
        }
    }

    /// The corner ornament has to finish before the vine starts, or the two
    /// collide in the one place the frame is busiest. `forged_frame` holds the
    /// vines off the corners by `FRAME_INSET * 1.6` for exactly this reason.
    #[test]
    fn the_corner_ornament_clears_the_stretch_the_vine_starts_in() {
        let clearance = FRAME_INSET as f64 * 1.6;
        for corner in 0..4 {
            let (eye, sx, sy) = corner_anchor(W, H, corner);
            for (i, volute) in corner_volutes(eye, sx, sy).iter().enumerate() {
                for p in sample(volute) {
                    let along = if i == 0 {
                        (p.x - eye.x) * sx
                    } else {
                        (p.y - eye.y) * sy
                    };
                    assert!(
                        along < clearance,
                        "corner {corner} volute {i}: reaches {along:.2} along the \
                         rail, into the vine's run at {clearance:.2}"
                    );
                }
            }
        }
    }

    /// A run of vine long enough to have several repeats, on the same band the
    /// side rails give it.
    fn a_run() -> Rinceau {
        rinceau(
            Point::new(96.0, 300.0),
            Point::new(596.0, 300.0),
            FRAME_INSET as f64 * 0.35,
            0,
        )
        .expect("500px is long enough for a run of vine")
    }

    /// How far a point is from a path.
    fn distance_to(path: &BezPath, p: Point) -> f64 {
        use floem::peniko::kurbo::ParamCurveNearest;
        path.segments()
            .map(|seg| seg.nearest(p, 1e-9).distance_sq.sqrt())
            .fold(f64::MAX, f64::min)
    }

    /// Where a piece of ornament begins — its root.
    fn springs_from(path: &BezPath) -> Point {
        use floem::peniko::kurbo::PathEl;
        match path.elements().first() {
            Some(PathEl::MoveTo(p)) => *p,
            other => panic!("a drawn ornament must open with a MoveTo, got {other:?}"),
        }
    }

    /// `rinceau`'s own documentation says the stem is "a single continuous
    /// serpentine through every repeat rather than ... a row of stamps". The
    /// first version contradicted its own comment. Each repeat was a complete
    /// S — control points at `+0.40h` and then `-0.40h` — and `side` flipped
    /// between repeats, so the curve *arrived* at each node heading one way
    /// across the run and *left* it heading the other. A cusp every twenty
    /// pixels, half of them hidden under a berry.
    ///
    /// That is most of why the run read as scattered marks: the eye integrates
    /// a continuous line into one object and gives up on a broken one.
    #[test]
    fn the_vine_stem_runs_smoothly_from_one_repeat_into_the_next() {
        let vine = a_run();
        assert!(
            vine.stem.len() >= 4,
            "only {} repeats — too few joints to be checking",
            vine.stem.len()
        );
        for (i, pair) in vine.stem.windows(2).enumerate() {
            let (arrive, leave) = (pair[0].p3 - pair[0].p2, pair[1].p1 - pair[1].p0);
            let turn = (arrive.x * leave.y - arrive.y * leave.x) / (arrive.hypot() * leave.hypot());
            assert!(
                arrive.x * leave.x + arrive.y * leave.y > 0.0,
                "joint {i}: the stem doubles back on itself"
            );
            assert!(
                turn.abs() < 1e-9,
                "joint {i}: the stem kinks — it arrives on {arrive:?} and leaves on {leave:?}"
            );
        }
    }

    /// Jones's eleventh proposition made checkable: every ornament traced to
    /// its branch and root. Nothing on a vine floats.
    ///
    /// Both halves of this were broken, in different disguises. Leaves were
    /// placed at a fixed offset from the run's *axis* instead of at a point on
    /// the stem, which left their bases about `0.16h` off it — further than
    /// the stem's own width. Tendrils were worse: `log_spiral` takes the eye
    /// of the spiral, and the attachment point was passed as that eye, so
    /// every tendril was drawn a full radius clear of the stem and touched it
    /// nowhere at all.
    ///
    /// A vine whose leaves are not on its stem is confetti by construction,
    /// however well each individual mark is drawn.
    ///
    /// What is measured is where each ornament *starts*, and that is not
    /// fussiness. The first version of this test asked whether any part of the
    /// ornament came near the stem, and a tendril spiralling *about* a point
    /// on the stem passes that — its tail curls inward toward the very point
    /// it is centred on. So the buggy tendril satisfied it. A root is a
    /// particular point, not a proximity.
    #[test]
    fn every_leaf_and_tendril_springs_from_a_point_on_the_stem() {
        let vine = a_run();
        let stem = vine.stem_path();
        // The stem's ink reaches half its width either side of the curve, so
        // that is what "on the stem" means.
        let reach = vine.stem_width / 2.0;

        for (i, l) in vine.leaves.iter().enumerate() {
            let d = distance_to(&stem, springs_from(&l.outline()));
            assert!(
                d <= reach,
                "leaf {i} is rooted {d:.2} from the stem it grows out of, \
                 which reaches {reach:.2}"
            );
        }
        for (i, t) in vine.tendrils.iter().enumerate() {
            let d = distance_to(&stem, springs_from(&t.path()));
            assert!(
                d <= reach,
                "tendril {i} is rooted {d:.2} from the stem it springs from, \
                 which reaches {reach:.2}"
            );
        }
    }

    /// The Rule of Alternation again: the vine rides the frieze, and a frieze
    /// is a member with edges. Ornament that swings onto the ovolo above and
    /// the scotia below stops reading as a band running along the frame and
    /// starts reading as marks strewn across it.
    ///
    /// The first version sized itself from `band * 0.9` and then measured
    /// every offset from the *centre* line, so it was built to a height of
    /// nearly the whole member and then given that much again on both sides.
    #[test]
    fn the_whole_rinceau_stays_on_the_frieze_it_rides() {
        let band = FRAME_INSET as f64 * 0.35;
        let vine = a_run();
        let axis = 300.0; // the run in `a_run` is horizontal at this y
        let half = band / 2.0;

        // The worst offender, not the first: the number in the message is the
        // one worth knowing when retuning.
        let mut worst = ("nothing", 0.0_f64);
        let mut note = |what: &'static str, across: f64, ink: f64| {
            if across + ink > worst.1 {
                worst = (what, across + ink);
            }
        };

        for p in sample(&vine.stem_path()) {
            note("the stem", (p.y - axis).abs(), vine.stem_width / 2.0);
        }
        for l in &vine.leaves {
            for p in sample(&l.outline()) {
                note("a leaf", (p.y - axis).abs(), 0.8 / 2.0);
            }
        }
        for t in &vine.tendrils {
            for p in sample(&t.path()) {
                note("a tendril", (p.y - axis).abs(), VINE_TENDRIL_INLAY / 2.0);
            }
        }
        for b in &vine.berries {
            note("a berry", (b.at.y - axis).abs() + b.r, 1.1);
        }

        let (what, reach) = worst;
        assert!(
            reach <= half,
            "{what} reaches {reach:.2} across a frieze that is only {half:.2} \
             from its centre line to its edge"
        );
    }

    /// A curl has to be bigger than the line that draws it. Below about a
    /// stroke and a half of radius a spiral stops resolving and lands as a
    /// blob, which is the same limit that stops the stones being faceted at
    /// small sizes.
    ///
    /// This guards a regression rather than the original defect: scaling the
    /// vine down onto its own frieze shrank every part of it, and the tendril
    /// is the part that reached the limit first — briefly ending up with a
    /// radius *narrower* than its own stroke.
    #[test]
    fn a_tendril_is_round_enough_to_read_as_a_curl() {
        let vine = a_run();
        for (i, t) in vine.tendrils.iter().enumerate() {
            assert!(
                t.r >= VINE_TENDRIL_INLAY * 1.5,
                "tendril {i} has a radius of {:.2} drawn with a {VINE_TENDRIL_INLAY:.2} \
                 stroke — that is a dot, not a curl",
                t.r
            );
        }
    }

    /// Under one light the four rails cannot share a brightness — that is most
    /// of what separates a frame with mass from a coloured outline.
    #[test]
    fn the_rails_are_lit_differently() {
        assert!(side_light(Side::Top) > side_light(Side::Left));
        assert!(side_light(Side::Left) > side_light(Side::Right));
        assert!(side_light(Side::Right) > side_light(Side::Bottom));
    }
}
