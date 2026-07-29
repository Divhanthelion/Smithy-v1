//! A configurable keyboard shortcut, stored as text.
//!
//! Written for the microphone but not about it: a hotkey is a hotkey, and the
//! next thing that wants one should not have to reinvent parsing, rendering and
//! collision-checking.
//!
//! Stored as the string a person would type — `cmd+shift+v` — rather than as a
//! serialised struct, because the file is edited by hand more often than it is
//! edited by the application, and a keycode is not something anyone can read.

use std::path::{Path, PathBuf};

use floem::prelude::{KeyboardEvent, Modifiers};

/// A modifier-plus-key combination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hotkey {
    /// The character, lowercased. Only single characters — this is a shortcut,
    /// not a chord.
    pub key: String,
    /// `⌘` on macOS, `Ctrl` elsewhere. Held together because the rest of the
    /// application already treats them as one, and a shortcut that meant
    /// different things on two platforms would be worse than either.
    pub cmd: bool,
    pub shift: bool,
    pub alt: bool,
}

impl Default for Hotkey {
    /// `⌘⇧V` — V for voice.
    ///
    /// Chosen to miss everything Smithy already claims: `⌘S`, `⌘O`, `⌃K`, `⌘B`,
    /// `⌘L` and `⌃\``. That is asserted rather than eyeballed, in
    /// `the_default_hotkey_does_not_collide_with_an_existing_shortcut` — a
    /// clash would silently shadow whichever handler ran second, and finding
    /// out which is not a debugging session anyone should have.
    fn default() -> Self {
        Self {
            key: "v".to_string(),
            cmd: true,
            shift: true,
            alt: false,
        }
    }
}

impl Hotkey {
    /// Parse `cmd+shift+v`, in any order and any case.
    ///
    /// `None` for anything that is not one key with optional modifiers —
    /// including a bare modifier, which would fire on its own and make the
    /// keyboard unusable.
    pub fn parse(text: &str) -> Option<Self> {
        let mut hotkey = Self {
            key: String::new(),
            cmd: false,
            shift: false,
            alt: false,
        };

        for part in text.split('+') {
            match part.trim().to_ascii_lowercase().as_str() {
                "" => continue,
                "cmd" | "command" | "meta" | "super" | "ctrl" | "control" => hotkey.cmd = true,
                "shift" => hotkey.shift = true,
                "alt" | "option" | "opt" => hotkey.alt = true,
                other if other.chars().count() == 1 => {
                    // A second key is a typo, not a chord.
                    if !hotkey.key.is_empty() {
                        return None;
                    }
                    other.clone_into(&mut hotkey.key);
                }
                _ => return None,
            }
        }

        (!hotkey.key.is_empty()).then_some(hotkey)
    }

    /// The stored form, which is also the form a person types.
    pub fn as_text(&self) -> String {
        let mut parts = Vec::new();
        if self.cmd {
            parts.push("cmd");
        }
        if self.shift {
            parts.push("shift");
        }
        if self.alt {
            parts.push("alt");
        }
        parts.push(&self.key);
        parts.join("+")
    }

    /// How it is shown on screen.
    ///
    /// Every glyph here is one Menlo carries — see `design::glyph`, and the
    /// test that scans for anything that would render as a box.
    pub fn describe(&self) -> String {
        let mut out = String::new();
        if self.cmd {
            out.push('⌘');
        }
        if self.shift {
            out.push('⇧');
        }
        if self.alt {
            out.push('⌥');
        }
        out.push_str(&self.key.to_uppercase());
        out
    }

    /// Whether a key event is this shortcut.
    ///
    /// `cmd` accepts either `⌘` or `Ctrl`, matching what the rest of the
    /// application does — the on-screen hints cannot always render the modifier
    /// glyph, so refusing the one somebody reached for is the worse failure.
    pub fn matches(&self, event: &KeyboardEvent) -> bool {
        let modifiers = event.modifiers;
        let cmd = modifiers.contains(Modifiers::META) || modifiers.contains(Modifiers::CONTROL);
        if cmd != self.cmd
            || modifiers.contains(Modifiers::SHIFT) != self.shift
            || modifiers.contains(Modifiers::ALT) != self.alt
        {
            return false;
        }
        // Shift changes the character the platform reports — `⇧V` arrives as
        // "V" on one keyboard and "v" on another — so compare case-insensitively
        // rather than trusting either.
        matches!(&event.key, floem::prelude::Key::Character(c)
            if c.eq_ignore_ascii_case(&self.key))
    }

    fn file_in(data_dir: &Path) -> PathBuf {
        data_dir.join("voice-hotkey")
    }

    /// Read the stored shortcut, falling back to the default.
    ///
    /// Never fails. A hotkey file somebody has mistyped should cost them their
    /// shortcut, not their editor.
    pub fn load(data_dir: &Path) -> Self {
        std::fs::read_to_string(Self::file_in(data_dir))
            .ok()
            .and_then(|text| Self::parse(text.trim()))
            .unwrap_or_default()
    }

    /// Persist it.
    pub fn save(&self, data_dir: &Path) -> Result<(), String> {
        std::fs::create_dir_all(data_dir).map_err(|e| e.to_string())?;
        std::fs::write(Self::file_in(data_dir), self.as_text()).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hotkey_survives_being_written_down_and_read_back() {
        for text in ["cmd+shift+v", "alt+m", "cmd+k", "shift+alt+cmd+z"] {
            let parsed = Hotkey::parse(text).unwrap_or_else(|| panic!("{text} should parse"));
            assert_eq!(
                Hotkey::parse(&parsed.as_text()),
                Some(parsed),
                "{text} did not survive a round trip"
            );
        }
    }

    /// Written by hand as often as by the application, so it takes what a
    /// person would actually type.
    #[test]
    fn the_spellings_people_use_are_all_accepted() {
        let expected = Hotkey {
            key: "v".into(),
            cmd: true,
            shift: true,
            alt: false,
        };
        for text in [
            "cmd+shift+v",
            "Cmd+Shift+V",
            "COMMAND+SHIFT+V",
            "shift+cmd+v",
            "ctrl+shift+v",
            " meta + shift + v ",
        ] {
            assert_eq!(Hotkey::parse(text).as_ref(), Some(&expected), "{text}");
        }
    }

    /// A bare modifier would fire on its own and make the keyboard unusable;
    /// two keys is a typo rather than a chord. Both are refused rather than
    /// half-understood.
    #[test]
    fn nonsense_is_refused_rather_than_guessed_at() {
        for text in ["", "cmd", "shift+alt", "cmd+shift", "cmd+v+b", "cmd+enter"] {
            assert_eq!(Hotkey::parse(text), None, "{text:?} should not parse");
        }
    }

    /// **The default must not shadow an existing shortcut.** Two handlers on
    /// one combination means whichever runs second silently loses, and working
    /// out which is not a debugging session anyone should have. These are the
    /// shortcuts `main.rs` claims in its root key handler.
    #[test]
    fn the_default_hotkey_does_not_collide_with_an_existing_shortcut() {
        // (key, cmd, shift) as the root handler matches them.
        let taken = [
            ("s", true, false),
            ("o", true, false),
            ("k", true, false),
            ("b", true, false),
            ("l", true, false),
            ("`", true, false),
            ("'", true, false),
        ];
        let default = Hotkey::default();
        for (key, cmd, shift) in taken {
            let clash = default.key == key && default.cmd == cmd && default.shift == shift;
            assert!(!clash, "the default hotkey shadows the existing {key:?}");
        }
    }

    /// The hint is drawn with the same font as everything else, so its glyphs
    /// have to be ones Menlo carries — otherwise the shortcut renders as a row
    /// of boxes, which has happened here twice.
    #[test]
    fn the_written_form_uses_only_glyphs_the_font_has() {
        let described = Hotkey::default().describe();
        assert_eq!(described, "⌘⇧V");
        let everything = Hotkey {
            key: "z".into(),
            cmd: true,
            shift: true,
            alt: true,
        };
        for ch in everything.describe().chars() {
            assert!(
                ch.is_ascii() || "⌘⇧⌥".contains(ch),
                "{ch} (U+{:04X}) is not a checked glyph",
                ch as u32
            );
        }
    }

    #[test]
    fn a_mistyped_file_falls_back_to_the_default_rather_than_breaking() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(
            Hotkey::load(tmp.path()),
            Hotkey::default(),
            "nothing stored"
        );

        std::fs::write(tmp.path().join("voice-hotkey"), "not a hotkey at all").unwrap();
        assert_eq!(Hotkey::load(tmp.path()), Hotkey::default(), "unparseable");

        let chosen = Hotkey::parse("alt+m").unwrap();
        chosen.save(tmp.path()).unwrap();
        assert_eq!(Hotkey::load(tmp.path()), chosen);
    }
}
