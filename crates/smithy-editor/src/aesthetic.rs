//! Which visual treatment the interface wears.
//!
//! Two complete looks, switched at runtime rather than themed by swapping
//! colour tokens. [`Aesthetic::Flat`] is the working editor: quiet surfaces,
//! elevation by value, everything subordinate to the code. [`Aesthetic::Forged`]
//! is ornamental — a carved frame, organic panel chrome, circuitry behind the
//! editor — and is deliberately a *different view tree* rather than the same
//! tree in different colours, because the ornament is structure, not paint.
//!
//! The choice lives here, in a crate the agent core does not depend on, so the
//! headless side of the application never learns that an interface exists.

use std::path::{Path, PathBuf};

/// The interface's visual treatment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Aesthetic {
    /// The working editor. Flat surfaces, minimal chrome.
    #[default]
    Flat,
    /// Ornamented: carved frame, organic panels, circuitry.
    Forged,
}

impl Aesthetic {
    /// The label shown in the menu.
    pub fn label(self) -> &'static str {
        match self {
            Aesthetic::Flat => "Flat",
            Aesthetic::Forged => "Forged",
        }
    }

    /// The other one. The switch is a toggle, not a list.
    pub fn toggled(self) -> Self {
        match self {
            Aesthetic::Flat => Aesthetic::Forged,
            Aesthetic::Forged => Aesthetic::Flat,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Aesthetic::Flat => "flat",
            Aesthetic::Forged => "forged",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "flat" => Some(Aesthetic::Flat),
            "forged" => Some(Aesthetic::Forged),
            _ => None,
        }
    }

    /// Where the preference is stored, given the app's data directory.
    pub fn file_in(data_dir: &Path) -> PathBuf {
        data_dir.join("aesthetic")
    }

    /// Read the stored preference, falling back to the default.
    ///
    /// Never fails: an unreadable or unrecognised file means the default, on
    /// the grounds that a corrupt preference should cost you your ornament,
    /// not your editor.
    pub fn load(data_dir: &Path) -> Self {
        std::fs::read_to_string(Self::file_in(data_dir))
            .ok()
            .and_then(|s| Self::parse(&s))
            .unwrap_or_default()
    }

    /// Persist the preference. Errors are returned rather than logged so the
    /// caller can decide whether a failure to remember is worth surfacing.
    pub fn save(self, data_dir: &Path) -> Result<(), String> {
        std::fs::create_dir_all(data_dir).map_err(|e| e.to_string())?;
        std::fs::write(Self::file_in(data_dir), self.as_str()).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_preference_survives_a_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(Aesthetic::load(tmp.path()), Aesthetic::Flat, "default");

        Aesthetic::Forged.save(tmp.path()).unwrap();
        assert_eq!(Aesthetic::load(tmp.path()), Aesthetic::Forged);

        Aesthetic::Flat.save(tmp.path()).unwrap();
        assert_eq!(Aesthetic::load(tmp.path()), Aesthetic::Flat);
    }

    /// A preference file written by a future version, or corrupted, must not
    /// stop the editor opening.
    #[test]
    fn an_unreadable_preference_falls_back_rather_than_failing() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(Aesthetic::file_in(tmp.path()), "iridescent").unwrap();
        assert_eq!(Aesthetic::load(tmp.path()), Aesthetic::Flat);

        // A directory where the file should be is unreadable as a string.
        let tmp2 = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(Aesthetic::file_in(tmp2.path())).unwrap();
        assert_eq!(Aesthetic::load(tmp2.path()), Aesthetic::Flat);
    }

    #[test]
    fn toggling_twice_returns_to_where_it_started() {
        for start in [Aesthetic::Flat, Aesthetic::Forged] {
            assert_eq!(start.toggled().toggled(), start);
            assert_ne!(start.toggled(), start);
        }
    }
}
