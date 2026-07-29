//! Design tokens.
//!
//! The first pass used Catppuccin Mocha's palette directly, and it read as a
//! rough-in: `BASE` (#1e1e2e) is a fairly light, distinctly violet grey, so
//! every panel landed on nearly the same value and the UI had no depth. Borders
//! were doing all the separation work, which is what makes an interface look
//! wireframed.
//!
//! This palette keeps the mood — dark, slightly cool, Catppuccin-adjacent
//! accents — but fixes three things:
//!
//! 1. **Real elevation.** Five background steps from [`BG_DEEP`] to
//!    [`BG_FLOAT`], each a visible but quiet increment. Surfaces separate by
//!    *value*, and borders become an accent rather than the structure.
//! 2. **Deeper, less violet base.** The greys carry a slight blue cast instead
//!    of a purple one, so the lavender accent reads as an accent rather than as
//!    more of the background.
//! 3. **A real text ramp.** Four weights of foreground with genuine contrast
//!    gaps, so hierarchy comes from the type rather than from font sizes.
//!
//! Spacing and type are scales, not free numbers. Every padding in the UI
//! should be one of [`SPACE_1`]–[`SPACE_6`] and every size one of the `TEXT_*`
//! constants; that consistency is most of what separates "designed" from
//! "assembled".

use floem::peniko::Color;

// ============================================================================
// Elevation — backgrounds, darkest to lightest
// ============================================================================

/// Behind everything. Window chrome, the menu bar.
pub const BG_DEEP: Color = Color::from_rgb8(13, 15, 20);
/// Secondary surfaces: side panels, the agent panel.
pub const BG_SUNKEN: Color = Color::from_rgb8(17, 20, 27);
/// The primary work surface: the editor.
pub const BG_BASE: Color = Color::from_rgb8(22, 25, 34);
/// Raised elements: input fields, hovered rows, inline code.
pub const BG_RAISED: Color = Color::from_rgb8(30, 34, 45);
/// Floating elements: menus, modals, dropdowns.
pub const BG_FLOAT: Color = Color::from_rgb8(37, 42, 55);

/// Hover wash, applied over whatever is beneath.
pub const BG_HOVER: Color = Color::from_rgba8(255, 255, 255, 10);
/// Selected-row background.
pub const BG_SELECTED: Color = Color::from_rgba8(137, 180, 250, 26);

// ============================================================================
// Borders
// ============================================================================

/// Barely-there division. The default: elevation should do the work.
pub const BORDER_SUBTLE: Color = Color::from_rgb8(30, 34, 44);
/// A deliberate edge — floating panels, focused inputs.
pub const BORDER: Color = Color::from_rgb8(45, 51, 66);
/// Focus ring.
pub const BORDER_FOCUS: Color = Color::from_rgb8(137, 180, 250);

// ============================================================================
// Foreground ramp
// ============================================================================

/// Primary reading text.
pub const FG: Color = Color::from_rgb8(214, 221, 235);
/// Secondary: labels, metadata that still needs to be read.
pub const FG_MUTED: Color = Color::from_rgb8(148, 158, 178);
/// Tertiary: hints, placeholders, things present but not for reading now.
pub const FG_FAINT: Color = Color::from_rgb8(96, 105, 124);
/// Quaternary: separators rendered as text, disabled glyphs.
pub const FG_GHOST: Color = Color::from_rgb8(66, 74, 90);

// ============================================================================
// Accents
// ============================================================================

/// The single primary accent. Used sparingly — an accent everywhere is not one.
pub const ACCENT: Color = Color::from_rgb8(137, 180, 250);
pub const ACCENT_HOVER: Color = Color::from_rgb8(166, 200, 255);
/// Text on top of a filled accent.
pub const ON_ACCENT: Color = Color::from_rgb8(13, 15, 20);

pub const SUCCESS: Color = Color::from_rgb8(148, 214, 158);
pub const WARN: Color = Color::from_rgb8(232, 194, 128);
pub const DANGER: Color = Color::from_rgb8(238, 132, 148);
pub const INFO: Color = Color::from_rgb8(137, 200, 220);
/// For the model's reasoning channel and other secondary machine output.
pub const THINKING: Color = Color::from_rgb8(160, 140, 200);

// ============================================================================
// Severity — the audit panel
// ============================================================================

pub const SEV_CRITICAL: Color = Color::from_rgb8(240, 110, 130);
pub const SEV_HIGH: Color = Color::from_rgb8(238, 158, 120);
pub const SEV_MEDIUM: Color = Color::from_rgb8(226, 197, 128);
pub const SEV_LOW: Color = Color::from_rgb8(134, 180, 214);
pub const SEV_INFO: Color = Color::from_rgb8(120, 130, 150);

// ============================================================================
// Syntax
// ============================================================================

pub mod syntax {
    use super::Color;

    pub const KEYWORD: Color = Color::from_rgb8(198, 160, 246);
    pub const STRING: Color = Color::from_rgb8(166, 218, 149);
    pub const NUMBER: Color = Color::from_rgb8(245, 169, 127);
    pub const COMMENT: Color = Color::from_rgb8(98, 108, 128);
    pub const FUNCTION: Color = Color::from_rgb8(138, 173, 244);
    pub const TYPE: Color = Color::from_rgb8(238, 212, 159);
    pub const VARIABLE: Color = Color::from_rgb8(202, 211, 245);
    pub const OPERATOR: Color = Color::from_rgb8(125, 196, 228);
    pub const PUNCTUATION: Color = Color::from_rgb8(147, 154, 183);
}

// ============================================================================
// Spacing scale
// ============================================================================

pub const SPACE_1: f32 = 4.0;
pub const SPACE_2: f32 = 8.0;
pub const SPACE_3: f32 = 12.0;
pub const SPACE_4: f32 = 16.0;
pub const SPACE_5: f32 = 24.0;
pub const SPACE_6: f32 = 32.0;

// ============================================================================
// Type scale
// ============================================================================

/// Metadata, shortcut hints, counters.
pub const TEXT_XS: f32 = 10.5;
/// Secondary UI text: tool arguments, status lines.
pub const TEXT_SM: f32 = 11.5;
/// The default for UI chrome.
pub const TEXT_BASE: f32 = 12.5;
/// Reading text: messages, answers.
pub const TEXT_MD: f32 = 13.5;
/// Section headings.
pub const TEXT_LG: f32 = 16.0;
/// Empty-state titles. Deliberately not enormous — the previous 32px
/// wordmark in the middle of the editor read as a splash screen.
pub const TEXT_XL: f32 = 21.0;

// ============================================================================
// Radii
// ============================================================================

/// Chips, small buttons.
pub const RADIUS_SM: f32 = 4.0;
/// Rows, inputs, standard buttons.
pub const RADIUS: f32 = 6.0;
/// Cards, dropdowns.
pub const RADIUS_LG: f32 = 9.0;

// ============================================================================
// Metrics
// ============================================================================

pub const MENU_BAR_HEIGHT: f32 = 30.0;
pub const TAB_BAR_HEIGHT: f32 = 36.0;
pub const ROW_HEIGHT: f32 = 24.0;
/// Monospace stack for code, the terminal, and anything that has to line up.
///
/// Named fonts first, the generic last. `"monospace"` on its own resolved to
/// **Courier**, whose cmap contains not one of the box-drawing, arrow or
/// geometric glyphs this UI draws with — which is why they all rendered as
/// missing-glyph boxes, in file content as well as in the chrome. Menlo has
/// every one of them.
///
/// This was measured, not guessed: the `cmap` tables of every installed font
/// were read and checked against the exact codepoints in use. Helvetica,
/// Helvetica Neue, Times, Geneva and Courier have essentially no symbol
/// coverage; Menlo has full coverage; Apple Symbols and Arial Unicode MS are
/// close. Do not replace this with a generic family again without repeating
/// that check.
pub const MONO: &str = "Menlo, Monaco, DejaVu Sans Mono, Consolas, monospace";

/// Font stack for labels whose *content is a glyph* rather than prose.
///
/// The same stack as [`MONO`], for a different reason. floem's default family
/// is sans, which resolves to Helvetica here, and Helvetica has no ⌘, ⌂, ↻, ←,
/// ▸, ▾, ❖, ✓ or ✕ at all. A label drawing an icon needs a family picked for
/// coverage, not for looks — separate constant so retuning the code font cannot
/// silently break every icon in the tree.
pub const SYMBOL: &str = MONO;

/// Every glyph the chrome draws, and the evidence that it can be drawn.
///
/// Menlo is first in [`SYMBOL`], so Menlo is what actually has to contain
/// these. The list below was checked by **reading Menlo's `cmap` table**, not by
/// looking at the screen — the same method that settled the last font bug here,
/// and for the same reason: a missing glyph renders as a box that is easy to
/// mistake for a deliberate square, and two of these sat in the UI for months
/// before anyone said so.
///
/// Two were missing when this list was first assembled:
///
/// - `⟲` U+27F2 ANTICLOCKWISE GAPPED CIRCLE ARROW — the agent panel's clear
///   button. Not in Menlo, Monaco, Courier or DejaVu Sans Mono. `↺` U+21BA is,
///   which is why the file browser's refresh arrow has always rendered and this
///   one never did.
/// - `⏹` U+23F9 BLACK SQUARE FOR STOP — the stopped-turn banner. Also absent,
///   and invisible in normal use because it only appears when a turn is
///   interrupted. `■` U+25A0 is present.
///
/// Emoji are deliberately not here. They resolve through the system's colour
/// emoji fallback rather than through this stack, so they render whatever the
/// named family contains.
pub mod glyph {
    /// Reset, revert, clear.
    pub const CLEAR: &str = "↺";
    /// Refresh, reload.
    pub const REFRESH: &str = "↻";
    /// Close, dismiss, failed.
    pub const CLOSE: &str = "✕";
    /// Succeeded.
    pub const OK: &str = "✓";
    /// Stopped.
    pub const STOP: &str = "■";
    /// Warning.
    pub const WARN: &str = "⚠";
    /// A status dot.
    pub const DOT: &str = "●";
    /// A hollow status dot.
    pub const RING: &str = "○";
    /// Disclosure, collapsed and expanded.
    pub const COLLAPSED: &str = "▸";
    pub const EXPANDED: &str = "▾";
    /// Fold everything up.
    pub const COLLAPSE_ALL: &str = "▴";
    /// Hide this panel.
    pub const HIDE: &str = "─";
    /// Home directory.
    pub const HOME: &str = "⌂";
    /// Up a level.
    pub const UP: &str = "←";
    /// A settings or config file.
    pub const CONFIG: &str = "⚙";
    /// A document.
    pub const DOCUMENT: &str = "❖";
    /// Sunrise and sunset, on the clock.
    pub const SUNRISE: &str = "↑";
    pub const SUNSET: &str = "↓";

    /// Every glyph above, for the test below.
    pub const ALL: &[&str] = &[
        CLEAR,
        REFRESH,
        CLOSE,
        OK,
        STOP,
        WARN,
        DOT,
        RING,
        COLLAPSED,
        EXPANDED,
        COLLAPSE_ALL,
        HIDE,
        HOME,
        UP,
        CONFIG,
        DOCUMENT,
        SUNRISE,
        SUNSET,
    ];
}

/// These assert relationships between the `const` tokens above, so clippy is
/// right that every one has a compile-time-known value — that is the point.
/// They exist to fail the build when someone retunes a token and breaks the
/// ramp, not to exercise runtime behaviour.
#[cfg(test)]
#[allow(clippy::assertions_on_constants)]
mod tests {
    use super::*;

    /// Byte accessors for the palette invariants.
    ///
    /// peniko's `Color` is now `AlphaColor<Srgb>`, holding components as f32 in
    /// 0.0..=1.0. Every invariant below is stated in 0–255 terms because that is
    /// how the palette was designed and how the hex literals read, so convert at
    /// the edge rather than restating thresholds in floating point — rewriting
    /// "no more than 14 units of blue over green" as 0.0549 would obscure what
    /// it means and invite a rounding argument.
    trait Bytes {
        fn r(&self) -> u8;
        fn g(&self) -> u8;
        fn b(&self) -> u8;
    }

    impl Bytes for Color {
        fn r(&self) -> u8 {
            self.to_rgba8().r
        }
        fn g(&self) -> u8 {
            self.to_rgba8().g
        }
        fn b(&self) -> u8 {
            self.to_rgba8().b
        }
    }

    fn luminance(c: Color) -> f64 {
        // Rec. 709 relative luminance, on the 0–255 components.
        0.2126 * c.r() as f64 + 0.7152 * c.g() as f64 + 0.0722 * c.b() as f64
    }

    /// Elevation only reads as depth if each step is actually lighter than the
    /// last. The original palette failed this — panels were all one value.
    #[test]
    fn background_steps_ascend() {
        let steps = [BG_DEEP, BG_SUNKEN, BG_BASE, BG_RAISED, BG_FLOAT];
        for pair in steps.windows(2) {
            assert!(
                luminance(pair[1]) > luminance(pair[0]),
                "each elevation step must be lighter than the one below it"
            );
        }
    }

    /// ...and by enough to actually see, without becoming stripes.
    #[test]
    fn background_steps_are_perceptible_but_quiet() {
        let steps = [BG_DEEP, BG_SUNKEN, BG_BASE, BG_RAISED, BG_FLOAT];
        for pair in steps.windows(2) {
            let delta = luminance(pair[1]) - luminance(pair[0]);
            assert!(delta >= 3.0, "step too subtle to register: {delta}");
            assert!(delta <= 22.0, "step too loud for a dark UI: {delta}");
        }
    }

    /// The foreground ramp has to be a ramp, or hierarchy collapses.
    #[test]
    fn foreground_ramp_descends() {
        let ramp = [FG, FG_MUTED, FG_FAINT, FG_GHOST];
        for pair in ramp.windows(2) {
            assert!(
                luminance(pair[0]) > luminance(pair[1]) + 20.0,
                "adjacent foreground steps must be clearly distinguishable"
            );
        }
    }

    #[test]
    fn primary_text_is_readable_on_every_surface() {
        for bg in [BG_DEEP, BG_SUNKEN, BG_BASE, BG_RAISED, BG_FLOAT] {
            let contrast = luminance(FG) - luminance(bg);
            assert!(contrast > 140.0, "primary text is too dim on {bg:?}");
        }
    }

    /// Faint text is meant to recede, but it still has to be legible.
    #[test]
    fn faint_text_remains_legible_on_the_base_surface() {
        assert!(luminance(FG_FAINT) - luminance(BG_BASE) > 40.0);
    }

    #[test]
    fn text_on_the_accent_is_dark_enough_to_read() {
        assert!(luminance(ACCENT) - luminance(ON_ACCENT) > 100.0);
    }

    /// The greys should be cool, not violet — that was what made the first pass
    /// look muddy against a lavender accent.
    #[test]
    fn greys_are_cool_rather_than_violet() {
        for bg in [BG_DEEP, BG_SUNKEN, BG_BASE, BG_RAISED, BG_FLOAT] {
            assert!(
                bg.b() >= bg.r(),
                "a cool grey should not be redder than it is blue"
            );
            assert!(
                bg.b() as i16 - bg.g() as i16 <= 14,
                "too much blue over green reads as violet"
            );
        }
    }

    #[test]
    fn severity_colours_are_all_distinct() {
        let sevs = [SEV_CRITICAL, SEV_HIGH, SEV_MEDIUM, SEV_LOW, SEV_INFO];
        for (i, a) in sevs.iter().enumerate() {
            for b in &sevs[i + 1..] {
                let distance = (a.r() as i32 - b.r() as i32).abs()
                    + (a.g() as i32 - b.g() as i32).abs()
                    + (a.b() as i32 - b.b() as i32).abs();
                assert!(
                    distance > 40,
                    "severity colours {a:?} and {b:?} are too close"
                );
            }
        }
    }

    /// The serious severities run red → orange → amber.
    ///
    /// Measured on the green channel rather than `r - b`: all three are
    /// red-dominant, and it is the amount of green that turns red into orange
    /// and orange into amber. An `r - b` metric calls orange "warmer" than red,
    /// which is the opposite of how the scale reads.
    #[test]
    fn the_serious_severities_run_red_to_amber() {
        assert!(
            SEV_CRITICAL.g() < SEV_HIGH.g(),
            "critical should be redder than high"
        );
        assert!(
            SEV_HIGH.g() < SEV_MEDIUM.g(),
            "high should be redder than medium"
        );
        for c in [SEV_CRITICAL, SEV_HIGH, SEV_MEDIUM] {
            assert!(c.r() > c.b(), "the serious severities should all be warm");
        }
        assert!(SEV_LOW.b() > SEV_LOW.r(), "low should be cool, not warm");
    }

    /// Below medium the scale stops being a heat ramp: `LOW` is cool blue and
    /// `INFO` is deliberately near-neutral, so it recedes rather than competing
    /// for attention. Asserting a single warm→cool ordering across all five
    /// would force `INFO` to be tinted for no reason.
    #[test]
    fn info_is_the_least_saturated_severity() {
        let saturation = |c: Color| {
            let (r, g, b) = (c.r() as i32, c.g() as i32, c.b() as i32);
            *[r, g, b].iter().max().unwrap() - *[r, g, b].iter().min().unwrap()
        };
        for other in [SEV_CRITICAL, SEV_HIGH, SEV_MEDIUM, SEV_LOW] {
            assert!(
                saturation(SEV_INFO) < saturation(other),
                "INFO should recede: it is more saturated than {other:?}"
            );
        }
    }

    #[test]
    fn the_spacing_scale_ascends() {
        let scale = [SPACE_1, SPACE_2, SPACE_3, SPACE_4, SPACE_5, SPACE_6];
        for pair in scale.windows(2) {
            assert!(pair[1] > pair[0]);
        }
    }

    #[test]
    fn the_type_scale_ascends() {
        let scale = [TEXT_XS, TEXT_SM, TEXT_BASE, TEXT_MD, TEXT_LG, TEXT_XL];
        for pair in scale.windows(2) {
            assert!(pair[1] > pair[0]);
        }
    }

    /// The empty-state title was 32px and read as a splash screen.
    #[test]
    fn the_largest_type_stays_restrained() {
        assert!(TEXT_XL <= 24.0, "an in-app heading above 24px shouts");
    }

    #[test]
    fn syntax_colours_are_readable_on_the_editor_surface() {
        use syntax::*;
        for c in [KEYWORD, STRING, NUMBER, FUNCTION, TYPE, VARIABLE, OPERATOR] {
            assert!(
                luminance(c) - luminance(BG_BASE) > 60.0,
                "syntax colour {c:?} is too dim to read on the editor background"
            );
        }
    }

    /// Comments should recede relative to code, but stay readable.
    #[test]
    fn comments_recede_without_disappearing() {
        use syntax::*;
        assert!(luminance(COMMENT) < luminance(VARIABLE));
        assert!(luminance(COMMENT) - luminance(BG_BASE) > 30.0);
    }
}

#[cfg(test)]
mod glyph_tests {
    use super::glyph;

    /// Characters Menlo carries that are text rather than icons — typography,
    /// accented letters in test data, and the keyboard symbols the shortcut
    /// hints draw. All confirmed present in Menlo's `cmap` alongside
    /// [`glyph::ALL`].
    const TEXT: &str = "°·×éö—…⇧⌃⌘";

    /// **Every non-ASCII character the UI can draw has to be one Menlo
    /// actually has.**
    ///
    /// Menlo is first in `SYMBOL`, and floem falls back to the system's colour
    /// emoji font for emoji but to a missing-glyph box for everything else. A
    /// box is easy to mistake for a deliberate square — `⟲` and `⏹` both sat in
    /// this UI unnoticed, and the one that was noticed had been wrong since it
    /// was written.
    ///
    /// This scans the crate's own source rather than a hand-kept list, so a new
    /// glyph pasted into any string literal anywhere trips it. If it fails,
    /// read Menlo's `cmap` before choosing a replacement — do not try glyphs
    /// until one looks right, because the box renders identically whatever is
    /// behind it.
    #[test]
    fn every_glyph_the_interface_draws_exists_in_the_font_that_draws_it() {
        let allowed: Vec<char> = glyph::ALL
            .iter()
            .flat_map(|g| g.chars())
            .chain(TEXT.chars())
            .collect();

        let mut offenders: Vec<(String, char)> = Vec::new();
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut stack = vec![root];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().is_none_or(|e| e != "rs") {
                    continue;
                }
                let Ok(text) = std::fs::read_to_string(&path) else {
                    continue;
                };
                // Test modules hold strings that are never drawn — `lsp::uri`
                // percent-encodes Japanese and accented text to prove it
                // round-trips, and none of it goes near a font. Skipping them
                // by brace depth rather than by filename, because the modules
                // are inline.
                let mut in_test = false;
                let mut depth = 0i32;

                for line in text.lines() {
                    if line.trim_start().starts_with("#[cfg(test)]") {
                        in_test = true;
                        depth = 0;
                    }
                    if in_test {
                        depth += line.matches('{').count() as i32;
                        depth -= line.matches('}').count() as i32;
                        if depth <= 0 && line.contains('}') {
                            in_test = false;
                        }
                        continue;
                    }
                    // Comments are prose and never reach the screen.
                    if line.trim_start().starts_with("//") {
                        continue;
                    }
                    // Only what is inside a string literal can be drawn.
                    for literal in line.split('"').skip(1).step_by(2) {
                        // `"\u{2500}"` is a glyph too, and is pure ASCII in the
                        // source — the first version of this scan looked only at
                        // literal characters and so was blind to every icon in
                        // the file browser, which writes all of them as escapes.
                        for ch in literal.chars().chain(unicode_escapes(literal)) {
                            let ordinary = ch.is_ascii() || is_emoji(ch);
                            if !ordinary && !allowed.contains(&ch) {
                                offenders.push((
                                    path.file_name().unwrap().to_string_lossy().into_owned(),
                                    ch,
                                ));
                            }
                        }
                    }
                }
            }
        }

        offenders.sort();
        offenders.dedup();
        assert!(
            offenders.is_empty(),
            "these would render as missing-glyph boxes — check Menlo's cmap, then \
             add them to `design::glyph`: {}",
            offenders
                .iter()
                .map(|(file, ch)| format!("{ch} (U+{:04X}) in {file}", *ch as u32))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    /// Every `\u{...}` escape in a string literal, decoded.
    fn unicode_escapes(literal: &str) -> Vec<char> {
        let mut out = Vec::new();
        let mut rest = literal;
        while let Some(start) = rest.find("\\u{") {
            rest = &rest[start + 3..];
            let Some(end) = rest.find('}') else { break };
            if let Ok(code) = u32::from_str_radix(&rest[..end], 16) {
                out.extend(char::from_u32(code));
            }
            rest = &rest[end..];
        }
        out
    }

    /// Emoji reach the screen through the system's colour emoji fallback rather
    /// than through `SYMBOL`, so they are outside this rule.
    ///
    /// Only the emoji planes, deliberately. An earlier version also exempted
    /// `U+2600..=U+27BF`, which is not an emoji block — it is Miscellaneous
    /// Symbols and Dingbats, and it holds `✓`, `✕`, `⚙` and most of the other
    /// icons this interface actually draws. Exempting it meant the check waved
    /// through the very characters it exists to examine.
    fn is_emoji(ch: char) -> bool {
        matches!(ch as u32, 0x1F000..=0x1FAFF | 0xFE0F)
    }
}
