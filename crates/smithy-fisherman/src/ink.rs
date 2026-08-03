//! The drawing surface the fisherman touches.
//!
//! Two drawing methods because that is genuinely all he uses — checked, not
//! assumed. A wider trait would be a wider thing to keep in sync, and the
//! whole point of this seam is that there is nothing to keep in sync.
//!
//! `begin` is the third method and a late addition: colour cannot separate
//! the figure from the hut (IRON sits on the HUT_ROOF→HUT_WALL line; their
//! midpoint is distance 2 from IRON), so the harness tags what is being
//! drawn instead. Default no-op — FloemInk inherits it and the live path
//! costs nothing.

use kurbo::BezPath;
use peniko::Color;

/// What `paint` is drawing right now.
///
/// The harness keeps a per-pixel mask keyed on this. Colour classification
/// cannot: IRON (17,20,27) lies almost on the HUT_ROOF (15,17,23) /
/// HUT_WALL (23,26,34) line, so every roof/wall AA edge read as figure ink
/// and the day-sheet crop framed indoor tiles on a stray pixel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Part {
    Hut,
    Figure,
    Fire,
    Props,
    Line,
    Smoke,
}

impl Part {
    /// Stable tag for the harness mask (1-based; 0 is "nothing").
    pub fn tag(self) -> u8 {
        match self {
            Part::Hut => 1,
            Part::Figure => 2,
            Part::Fire => 3,
            Part::Props => 4,
            Part::Line => 5,
            Part::Smoke => 6,
        }
    }

    pub fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            1 => Some(Part::Hut),
            2 => Some(Part::Figure),
            3 => Some(Part::Fire),
            4 => Some(Part::Props),
            5 => Some(Part::Line),
            6 => Some(Part::Smoke),
            _ => None,
        }
    }
}

/// Everything the fisherman needs to be drawn, and nothing else.
pub trait Ink {
    fn fill(&mut self, path: &BezPath, color: Color);
    fn stroke(&mut self, path: &BezPath, color: Color, width: f64);

    /// What is being drawn next. Default no-op: the app ignores it entirely.
    fn begin(&mut self, _part: Part) {}
}
