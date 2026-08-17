//! Floem glue for the fisherman on the forged rail.
//!
//! The poses, the hut, and `paint` live in `smithy-fisherman`. This module is
//! the seam: Aesthetic gate, session clock, and the floem `Ink` adapter.

pub use smithy_fisherman::fisherman::*;
pub use smithy_fisherman::Ink;

use floem::peniko::kurbo::{BezPath, Stroke};
use floem::peniko::Color;
use floem::prelude::*;
use floem::reactive::{RwSignal, SignalGet};
use floem::views::canvas;

use crate::aesthetic::Aesthetic;

/// The build is timed from the launch rather than from the wall clock, so it
/// happens when somebody is looking.
fn session_seconds() -> f64 {
    use std::sync::OnceLock;
    static START: OnceLock<std::time::Instant> = OnceLock::new();
    START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_secs_f64()
}

/// Local newtype so we can `impl Ink` here.
///
/// Ink lives in `smithy-fisherman` and PaintCx lives in floem — implementing a
/// foreign trait for a foreign type is an orphan-rule error. The plan's
/// `impl Ink for PaintCx` is this adapter; the newtype is the only legal home
/// for it once the trait moved out of the editor crate.
struct FloemInk<'a, 'b>(&'a mut floem::context::PaintCx<'b>);

impl Ink for FloemInk<'_, '_> {
    fn fill(&mut self, path: &BezPath, color: Color) {
        // Deref to the inner renderer: calling `self.0.fill` would still work,
        // but going through Deref keeps the call site identical to the old one.
        (**self.0).fill(path, color, 0.0);
    }

    fn stroke(&mut self, path: &BezPath, color: Color, width: f64) {
        (**self.0).stroke(path, color, &Stroke::new(width));
    }
}

/// The fisherman and his hut, on the frame's bottom rail.
///
/// He lives on the metalwork rather than on the terminal's tab bar, which is
/// where the plan first put him — the terminal panel is hidden by default, so
/// a fisherman who lives on it is a fisherman nobody ever sees. The rail is his
/// alone: the vine that ran along it was taken out to make room.
///
/// A canvas of his own, deliberately, because he animates several times a
/// second and nothing else in the window should repaint at his rate.
pub fn fisherman_view(aesthetic: RwSignal<Aesthetic>, tick: RwSignal<u64>) -> impl IntoView {
    canvas(move |cx, size| {
        let (w, h) = (size.width, size.height);
        let band = crate::forged::FRAME_INSET as f64;
        if aesthetic.get() != Aesthetic::Forged {
            return;
        }
        let frame = tick.get();
        // Tiny windows refuse him: paint would still draw, but the harness is
        // the place for undersized scenes, not a squeezed rail in the app.
        if w < 240.0 || h < band * 3.0 {
            return;
        }

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        let hours = crate::localtime::local_hours(now);
        let day = crate::localtime::local_day(now);
        let (sunrise, sunset) = crate::celestial::todays_sun(now);
        let scene = scene_at(
            w,
            h,
            band,
            hours,
            sunrise,
            sunset,
            day,
            session_seconds(),
            frame,
        );
        paint(&mut FloemInk(cx), &scene);
    })
    .style(|s| {
        s.absolute()
            .width_full()
            .height_full()
            .pointer_events_none()
    })
}
