//! A digital clock for the menu bar.
//!
//! Time is doing a lot of work in this application — the sky is drawn for the
//! real sun, the fisherman's whole day is anchored to sunrise and sunset, and
//! dictation is timestamped by nothing at all. When a backdrop starts depending
//! on the clock, being able to *see* the clock stops being decoration and
//! starts being how you check the backdrop.
//!
//! Off by default and remembered, because a clock is a preference and the
//! menu bar is not a dashboard.

use std::path::{Path, PathBuf};

/// How the time is written.
///
/// Twenty-four hour, zero-padded, seconds included. Seconds because the thing
/// most worth checking against this clock is whether something is *moving* —
/// a minute-resolution clock beside an animated backdrop tells you nothing for
/// fifty-nine seconds at a time.
pub fn format(hours_since_midnight: f64) -> String {
    let total = (hours_since_midnight.rem_euclid(24.0) * 3600.0).floor() as i64;
    let (h, m, s) = (total / 3600, (total / 60) % 60, total % 60);
    format!("{h:02}:{m:02}:{s:02}")
}

/// The time, and where the sun is in relation to it.
///
/// The sun times are what make the clock worth having beside this particular
/// application: they are the fisherman's anchors, so seeing them is seeing why
/// he is doing what he is doing.
pub fn format_with_sun(hours: f64, sunrise: f64, sunset: f64) -> String {
    format!("{}   ↑{} ↓{}", format(hours), short(sunrise), short(sunset))
}

/// Hours and minutes only — enough for a sunrise.
fn short(hours: f64) -> String {
    let total = (hours.rem_euclid(24.0) * 60.0).round() as i64;
    format!("{:02}:{:02}", (total / 60) % 24, total % 60)
}

/// Whether the clock is shown. Remembered between sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ClockVisible(pub bool);

impl ClockVisible {
    fn file_in(data_dir: &Path) -> PathBuf {
        data_dir.join("clock-visible")
    }

    /// Read the stored preference. Never fails: an unreadable file means the
    /// default, on the grounds that a corrupt preference should cost you a
    /// clock, not your editor.
    pub fn load(data_dir: &Path) -> bool {
        std::fs::read_to_string(Self::file_in(data_dir))
            .map(|text| text.trim() == "on")
            .unwrap_or(false)
    }

    pub fn save(data_dir: &Path, visible: bool) -> Result<(), String> {
        std::fs::create_dir_all(data_dir).map_err(|e| e.to_string())?;
        std::fs::write(Self::file_in(data_dir), if visible { "on" } else { "off" })
            .map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_time_is_written_the_way_a_clock_writes_it() {
        assert_eq!(format(0.0), "00:00:00");
        assert_eq!(format(9.5), "09:30:00");
        assert_eq!(format(13.0 + 45.0 / 60.0 + 30.0 / 3600.0), "13:45:30");
        assert_eq!(format(23.0 + 59.0 / 60.0 + 59.0 / 3600.0), "23:59:59");
    }

    /// Midnight is a boundary and boundaries are where clocks go wrong. A
    /// clock reading 24:00:00, or -01:00:00 a second before midnight, is the
    /// classic version of this.
    #[test]
    fn the_clock_wraps_at_midnight_rather_than_counting_past_it() {
        assert_eq!(format(24.0), "00:00:00");
        assert_eq!(format(25.5), "01:30:00");
        assert_eq!(format(-0.5), "23:30:00", "an hour before the epoch");

        for step in 0..(24 * 60) {
            let text = format(f64::from(step) / 60.0);
            let hour: i32 = text[..2].parse().expect("two digits of hour");
            assert!((0..24).contains(&hour), "{text} is not a time of day");
        }
    }

    /// The sun times are the fisherman's anchors, so the clock shows them —
    /// seeing them is seeing why he is doing what he is doing.
    #[test]
    fn the_clock_shows_where_the_sun_is() {
        let line = format_with_sun(14.5, 5.75, 20.25);
        assert!(line.starts_with("14:30:00"), "{line}");
        assert!(line.contains("05:45"), "sunrise missing from {line}");
        assert!(line.contains("20:15"), "sunset missing from {line}");
    }

    /// Every glyph in it has to be one Menlo carries, or the clock renders as
    /// boxes — which has happened twice in this interface already, once
    /// because the character was missing and once because the font was not
    /// asked for.
    #[test]
    fn the_clock_uses_only_glyphs_the_font_has() {
        for ch in format_with_sun(9.0, 6.0, 18.0).chars() {
            assert!(
                ch.is_ascii() || "↑↓".contains(ch),
                "{ch} (U+{:04X}) is not a checked glyph",
                ch as u32
            );
        }
    }

    #[test]
    fn the_preference_survives_a_round_trip_and_defaults_to_off() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!ClockVisible::load(tmp.path()), "off unless asked for");

        ClockVisible::save(tmp.path(), true).unwrap();
        assert!(ClockVisible::load(tmp.path()));

        ClockVisible::save(tmp.path(), false).unwrap();
        assert!(!ClockVisible::load(tmp.path()));

        std::fs::write(tmp.path().join("clock-visible"), "nonsense").unwrap();
        assert!(!ClockVisible::load(tmp.path()), "unreadable means off");
    }
}
