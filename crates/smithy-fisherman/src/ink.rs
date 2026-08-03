//! The drawing surface the fisherman touches.
//!
//! Two methods because that is genuinely all he uses — checked, not assumed.
//! A wider trait would be a wider thing to keep in sync, and the whole point
//! of this seam is that there is nothing to keep in sync.

use kurbo::BezPath;
use peniko::Color;

/// Everything the fisherman needs to be drawn, and nothing else.
pub trait Ink {
    fn fill(&mut self, path: &BezPath, color: Color);
    fn stroke(&mut self, path: &BezPath, color: Color, width: f64);
}
