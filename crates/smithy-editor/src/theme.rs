//! Theme.
//!
//! The palette lives in [`crate::design`]. This module keeps the historical
//! `catppuccin::*` names alive as aliases onto those tokens, because every view
//! in the crate already refers to them — remapping here restyles the whole
//! application coherently instead of leaving half of it on the old colours
//! while the other half is updated.
//!
//! New code should use [`crate::design`] directly. These names are a bridge,
//! not the interface: they name a hue (`MAUVE`, `PEACH`) rather than a role,
//! which is exactly the property that let the first pass drift.

/// Role-mapped aliases for the original Catppuccin Mocha names.
pub mod catppuccin {
    use crate::design;
    use floem::peniko::Color;

    // --- surfaces ---
    /// The editor surface.
    pub const BASE: Color = design::BG_BASE;
    /// Side panels.
    pub const MANTLE: Color = design::BG_SUNKEN;
    /// Window chrome, headers.
    pub const CRUST: Color = design::BG_DEEP;
    /// Raised: hovered rows, inputs, inline code.
    pub const SURFACE0: Color = design::BG_RAISED;
    /// Deliberate edges.
    pub const SURFACE1: Color = design::BORDER;
    /// Ghosted glyphs, counters.
    pub const SURFACE2: Color = design::FG_GHOST;

    // --- foreground ramp ---
    pub const TEXT: Color = design::FG;
    pub const SUBTEXT1: Color = design::FG_MUTED;
    pub const SUBTEXT0: Color = design::FG_MUTED;
    pub const OVERLAY2: Color = design::FG_MUTED;
    pub const OVERLAY1: Color = design::FG_FAINT;
    pub const OVERLAY0: Color = design::FG_FAINT;

    // --- accents, mapped to roles rather than hues ---
    pub const LAVENDER: Color = design::ACCENT;
    pub const BLUE: Color = design::ACCENT;
    pub const SAPPHIRE: Color = design::INFO;
    pub const SKY: Color = design::INFO;
    pub const TEAL: Color = design::SUCCESS;
    pub const GREEN: Color = design::SUCCESS;
    pub const YELLOW: Color = design::WARN;
    pub const PEACH: Color = design::SEV_HIGH;
    pub const MAROON: Color = design::DANGER;
    pub const RED: Color = design::DANGER;
    pub const MAUVE: Color = design::THINKING;
    pub const PINK: Color = design::THINKING;
    pub const FLAMINGO: Color = design::SEV_HIGH;
    pub const ROSEWATER: Color = design::FG;
}
