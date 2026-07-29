//! Fuzzy matching for the `edit` tool.
//!
//! ## The problem this solves
//!
//! coda's post-mortem named this as the single most likely real-world failure
//! and shipped without fixing it:
//!
//! > `edit` is measured reliable for single distinctive-line targets given exact
//! > bytes, but the **live `read → edit` path is untested** — `read` prepends
//! > `N⇥` line numbers the model must strip to form `old_string`. Most likely
//! > place real edits could fail.
//!
//! rustcoder independently built a general fuzzy cascade for the same class of
//! failure. Neither project has the whole answer: rustcoder's cascade never
//! knew about the line-number gutter, and coda knew about the gutter but had
//! only exact matching. Putting them together gives both.
//!
//! ## The cascade
//!
//! 1. **Exact** — plain substring match, tried first because it is the cheapest
//!    and the most certain.
//! 2. **Gutter strip** — if `old_string` looks like it was pasted straight out
//!    of `read` output, remove the `N⇥` prefix and try again. Cheap, and it
//!    fixes the failure coda predicted directly. (New here.)
//! 3. **Whitespace-normalized** — collapse whitespace runs and re-trim; catches
//!    re-indentation and tab/space drift. (from rustcoder)
//! 4. **Line fuzzy** — slide a window and score with `similar`; catches a
//!    dropped or reworded line, and is bounded — see [`Granularity::for_sweep`].
//!    (from rustcoder)
//!
//! rustcoder had a fourth embedding-based tier requiring a live embeddings
//! endpoint. It is deliberately not ported: it costs ~200ms and a network
//! round-trip per attempt, and tiers 1–4 already cover the failures coda
//! actually predicted. [`MatchTier::Embedding`] is retained so a later
//! implementation slots in without a breaking change.
//!
//! Every non-exact match reports **what actually matched** back to the model, so
//! the next edit in the same file uses the right text.

use similar::TextDiff;

/// Which tier produced a match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchTier {
    Exact,
    /// Exact, but only after stripping a `read`-style line-number gutter.
    GutterStripped,
    WhitespaceNormalized,
    LineFuzzy,
    /// Reserved. Not currently produced — see the module docs.
    Embedding,
}

impl std::fmt::Display for MatchTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            MatchTier::Exact => "exact",
            MatchTier::GutterStripped => "line-numbers-stripped",
            MatchTier::WhitespaceNormalized => "whitespace-normalized",
            MatchTier::LineFuzzy => "line-fuzzy",
            MatchTier::Embedding => "embedding",
        };
        f.write_str(s)
    }
}

/// A successful match against file content.
#[derive(Debug, Clone)]
pub struct FuzzyMatch {
    /// The text from the file that actually matched, byte for byte.
    pub matched_text: String,
    /// Byte offset in the file where the match starts.
    pub byte_offset: usize,
    pub tier: MatchTier,
    /// 0.0–1.0.
    pub confidence: f64,
    /// Whether this is confident enough to apply without asking.
    pub auto_apply: bool,
}

impl FuzzyMatch {
    pub fn byte_range(&self) -> std::ops::Range<usize> {
        self.byte_offset..self.byte_offset + self.matched_text.len()
    }

    /// A note for the model explaining a non-exact match, so its next `edit`
    /// against this file uses text that will match exactly.
    pub fn advisory(&self) -> Option<String> {
        if self.tier == MatchTier::Exact {
            return None;
        }
        Some(format!(
            "Note: `old_string` did not match exactly; resolved by {} match (confidence {:.2}). \
             The actual text in the file is:\n{}\n\
             Use that exact text for further edits to this region.",
            self.tier, self.confidence, self.matched_text
        ))
    }
}

/// Run the full cascade. Returns the first tier that matched.
pub fn find(content: &str, old_text: &str) -> Option<FuzzyMatch> {
    if old_text.is_empty() {
        return None;
    }

    // Tier 1: exact — cheapest and most certain, so it goes first.
    if let Some(offset) = content.find(old_text) {
        return Some(FuzzyMatch {
            matched_text: old_text.to_string(),
            byte_offset: offset,
            tier: MatchTier::Exact,
            confidence: 1.0,
            auto_apply: true,
        });
    }

    // Tier 2: the model pasted `read` output verbatim, gutter and all.
    if let Some(stripped) = strip_line_number_gutter(old_text) {
        if let Some(offset) = content.find(&stripped) {
            return Some(FuzzyMatch {
                matched_text: stripped,
                byte_offset: offset,
                tier: MatchTier::GutterStripped,
                confidence: 1.0,
                auto_apply: true,
            });
        }
        // The gutter was real but the payload still doesn't match exactly —
        // hand the stripped form to the fuzzier tiers rather than the raw one.
        if let Some(mut m) = fuzzy_tiers(content, &stripped) {
            m.confidence *= 0.99;
            return Some(m);
        }
    }

    fuzzy_tiers(content, old_text)
}

fn fuzzy_tiers(content: &str, old_text: &str) -> Option<FuzzyMatch> {
    try_whitespace_normalized(content, old_text).or_else(|| try_line_fuzzy(content, old_text))
}

/// Count how many times `old_text` matches exactly. Used for the uniqueness
/// check, which only applies to exact matches — a fuzzy match is by definition
/// a single best region.
pub fn count_exact(content: &str, old_text: &str) -> usize {
    if old_text.is_empty() {
        return 0;
    }
    content.matches(old_text).count()
}

// ============================================================================
// Tier 2: line-number gutter
// ============================================================================

/// Strip a `read`-style line-number gutter (`"%6d\t"`) if — and only if — every
/// line carries one, blank lines included (`read` numbers those too).
///
/// The all-or-nothing rule matters: source code legitimately contains lines like
/// `42\tfoo` inside string literals or TSV fixtures. Requiring *every* line to
/// have the prefix, and requiring the numbers to run consecutively, makes a
/// false positive essentially impossible.
///
/// Returns `None` when the input does not look like `read` output.
pub fn strip_line_number_gutter(text: &str) -> Option<String> {
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        return None;
    }

    let mut stripped = Vec::with_capacity(lines.len());
    let mut numbers = Vec::with_capacity(lines.len());

    for line in &lines {
        let (number, rest) = split_gutter(line)?;
        numbers.push(number);
        stripped.push(rest);
    }

    // Require the line numbers to be consecutive and ascending.
    if numbers.len() > 1 {
        for pair in numbers.windows(2) {
            if pair[1] != pair[0] + 1 {
                return None;
            }
        }
    }

    let mut out = stripped.join("\n");
    if text.ends_with('\n') {
        out.push('\n');
    }
    Some(out)
}

/// Split `"   42\tfn main() {"` into `(42, "fn main() {")`.
fn split_gutter(line: &str) -> Option<(u64, &str)> {
    let tab = line.find('\t')?;
    let (head, rest) = line.split_at(tab);
    let number: u64 = head.trim().parse().ok()?;
    Some((number, &rest[1..]))
}

// ============================================================================
// Tier 3: whitespace-normalized
// ============================================================================

fn normalize_whitespace(s: &str) -> String {
    s.lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Find `old_text` in `content` ignoring whitespace differences, then map the
/// hit back onto the original bytes.
///
/// The mapping is done by re-scanning candidate line windows rather than by
/// walking a normalized-to-original offset table (rustcoder's approach). The
/// table walk had to reconstruct which whitespace runs were leading, interior,
/// or trailing, and got the boundaries subtly wrong on lines with trailing
/// whitespace. Comparing normalized *line windows* is slower but is exact by
/// construction, because each window's byte range comes straight from the
/// original text.
pub fn try_whitespace_normalized(content: &str, old_text: &str) -> Option<FuzzyMatch> {
    let norm_old = normalize_whitespace(old_text);
    if norm_old.trim().is_empty() {
        return None;
    }

    let line_starts = line_start_offsets(content);
    let content_lines: Vec<&str> = content.lines().collect();
    let target_lines = old_text.lines().count().max(1);

    if target_lines > content_lines.len() {
        return None;
    }

    for start in 0..=(content_lines.len() - target_lines) {
        let end = start + target_lines;
        let window = &content_lines[start..end];
        if normalize_whitespace(&window.join("\n")) != norm_old {
            continue;
        }

        let byte_offset = line_starts[start];
        let last = window.last().unwrap();
        let end_offset = line_starts[end - 1] + last.len();

        return Some(FuzzyMatch {
            matched_text: content[byte_offset..end_offset].to_string(),
            byte_offset,
            tier: MatchTier::WhitespaceNormalized,
            confidence: 0.98,
            auto_apply: true,
        });
    }
    None
}

// ============================================================================
// Tier 4: line fuzzy
// ============================================================================

/// Slide a window over the file and score each with `similar`'s diff ratio.
pub fn try_line_fuzzy(content: &str, old_text: &str) -> Option<FuzzyMatch> {
    try_line_fuzzy_with_threshold(content, old_text, 0.85)
}

pub fn try_line_fuzzy_with_threshold(
    content: &str,
    old_text: &str,
    threshold: f64,
) -> Option<FuzzyMatch> {
    let content_lines: Vec<&str> = content.lines().collect();
    let old_len = old_text.lines().count();
    if old_len == 0 || content_lines.is_empty() {
        return None;
    }

    let line_starts = line_start_offsets(content);
    let min_window = old_len.saturating_sub(2).max(1);
    let max_window = (old_len + 2).min(content_lines.len());

    // Decided once for the whole sweep, not per comparison. See `Granularity`.
    let granularity =
        Granularity::for_sweep(old_text, content_lines.len(), max_window - min_window);

    let mut best: Option<(f64, usize, usize)> = None;

    for window_size in min_window..=max_window {
        if window_size > content_lines.len() {
            continue;
        }
        for start in 0..=(content_lines.len() - window_size) {
            let end = start + window_size;
            let window_text = content_lines[start..end].join("\n");
            let score = similarity(old_text, &window_text, granularity);
            if best.map(|(b, _, _)| score > b).unwrap_or(true) {
                best = Some((score, start, end));
            }
        }
    }

    let (score, start, end) = best?;
    if score < threshold {
        return None;
    }

    let byte_offset = line_starts[start];
    let last = content_lines[end - 1];
    let end_offset = line_starts[end - 1] + last.len();

    Some(FuzzyMatch {
        matched_text: content[byte_offset..end_offset].to_string(),
        byte_offset,
        tier: MatchTier::LineFuzzy,
        confidence: score,
        // A line-fuzzy hit rewrites text the model did not reproduce exactly.
        // Require a high score before doing that without review.
        auto_apply: score >= 0.95,
    })
}

/// What a single comparison in the sweep is scored over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Granularity {
    /// One character is one atom. Catches a one-character drift, which is the
    /// whole reason this tier exists.
    Chars,
    /// One line is one atom. Coarser, and vastly cheaper.
    Lines,
}

/// Roughly how many diff cells the sweep may spend before dropping to lines.
///
/// Calibrated against measurement rather than chosen: character diffing runs at
/// something like 3·10⁸ cells a second here, so this is a budget of about a
/// fifth of a second — slow enough to be worth having, fast enough that a failed
/// edit does not read as a hang.
const CHAR_DIFF_BUDGET: u64 = 50_000_000;

impl Granularity {
    /// Choose once, for the whole sweep.
    ///
    /// **The budget has to bound the sweep, not one comparison, and that was the
    /// defect.** The previous guard refused character diffing for any block over
    /// 8 KB, which is the right idea applied to the wrong quantity: the sweep
    /// runs one comparison per window *per starting line*, so the cost that
    /// matters is `windows × lines × old × window`, and every individual
    /// comparison in the expensive case is comfortably under 8 KB.
    ///
    /// Measured, against a 1500-line file, before this existed:
    ///
    /// | `old_string` | time |
    /// |---|---|
    /// | 2 lines | 0.2 s |
    /// | 20 lines | 8.1 s |
    /// | 40 lines | 29.8 s |
    /// | 60 lines | 63.4 s |
    ///
    /// A model pasting a block that does not match is not an edge case, and the
    /// turn it belongs to has no cancellation checkpoint inside a tool — so this
    /// was not a slow edit, it was an editor that stopped answering.
    fn for_sweep(old_text: &str, content_lines: usize, window_sizes: usize) -> Granularity {
        // Each window is about as long as the text being matched against it, so
        // `old · old` estimates one comparison's cell count.
        let per_comparison = (old_text.len() as u64).saturating_mul(old_text.len() as u64);
        let comparisons = (content_lines as u64).saturating_mul(window_sizes as u64 + 1);
        if comparisons.saturating_mul(per_comparison) <= CHAR_DIFF_BUDGET {
            Granularity::Chars
        } else {
            Granularity::Lines
        }
    }
}

/// Similarity of two text blocks, 0.0–1.0.
///
/// Scored over **characters** where the budget allows. rustcoder used
/// `TextDiff::from_lines(..).ratio()` unconditionally, which treats a whole line
/// as one atom: changing a single character makes that line a total mismatch, so
/// a two-line target with one edited character scores 0.5 and falls under any
/// useful threshold. Since a one-character drift is precisely the case this tier
/// exists to catch, line granularity defeats the purpose at small sizes.
///
/// At large sizes it is a reasonable proxy — a forty-line block that differs by
/// one line still scores 0.975 and auto-applies — and it is the difference
/// between a fifth of a second and a minute. See [`Granularity::for_sweep`].
fn similarity(a: &str, b: &str, granularity: Granularity) -> f64 {
    match granularity {
        Granularity::Chars => TextDiff::from_chars(a, b).ratio() as f64,
        Granularity::Lines => TextDiff::from_lines(a, b).ratio() as f64,
    }
}

/// Byte offset of the start of each line.
///
/// Computed from the original text rather than by summing `line.len() + 1`
/// (rustcoder's approach), which silently corrupts offsets on files with `\r\n`
/// line endings or without a trailing newline.
fn line_start_offsets(content: &str) -> Vec<usize> {
    let mut offsets = vec![0usize];
    for (i, b) in content.bytes().enumerate() {
        if b == b'\n' {
            offsets.push(i + 1);
        }
    }
    offsets
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "fn main() {\n    let retry_limit = 5;\n    println!(\"hi\");\n}\n";

    #[test]
    fn exact_match_wins() {
        let m = find(SAMPLE, "let retry_limit = 5;").unwrap();
        assert_eq!(m.tier, MatchTier::Exact);
        assert_eq!(m.confidence, 1.0);
        assert_eq!(&SAMPLE[m.byte_range()], "let retry_limit = 5;");
        assert!(m.advisory().is_none());
    }

    /// The exact failure coda predicted and never tested: the model copies
    /// `read` output straight into `old_string`, gutter included.
    #[test]
    fn strips_read_output_gutter() {
        let pasted = "     2\t    let retry_limit = 5;\n     3\t    println!(\"hi\");";
        let m = find(SAMPLE, pasted).unwrap();
        assert_eq!(m.tier, MatchTier::GutterStripped);
        assert_eq!(
            &SAMPLE[m.byte_range()],
            "    let retry_limit = 5;\n    println!(\"hi\");"
        );
        assert!(m.advisory().unwrap().contains("line-numbers-stripped"));
    }

    #[test]
    fn gutter_strip_requires_every_line_to_have_one() {
        let mixed = "     2\t    let retry_limit = 5;\n    println!(\"hi\");";
        assert!(strip_line_number_gutter(mixed).is_none());
    }

    #[test]
    fn gutter_strip_requires_consecutive_numbers() {
        let jumbled = "     2\t    let retry_limit = 5;\n    99\t    println!(\"hi\");";
        assert!(strip_line_number_gutter(jumbled).is_none());
    }

    /// Source that genuinely contains `number<TAB>text` must not be mangled.
    #[test]
    fn gutter_strip_ignores_tsv_like_content() {
        let tsv = "1\tapple\n7\tbanana";
        assert!(strip_line_number_gutter(tsv).is_none());
    }

    #[test]
    fn whitespace_normalized_match() {
        let m = find(SAMPLE, "let    retry_limit=5;").unwrap_or_else(|| {
            find(SAMPLE, "let retry_limit  =  5;").expect("should normalize whitespace")
        });
        assert_ne!(m.tier, MatchTier::Exact);
    }

    #[test]
    fn whitespace_match_maps_back_to_original_bytes() {
        let content = "struct A {\n\tfield: u8,   \n}\n";
        let m = try_whitespace_normalized(content, "field: u8,").unwrap();
        assert_eq!(&content[m.byte_range()], "\tfield: u8,   ");
    }

    #[test]
    fn line_fuzzy_tolerates_a_reworded_line() {
        let content = "fn a() {\n    let x = compute_value(1, 2);\n    return x;\n}\n";
        let m = try_line_fuzzy(content, "    let x = compute_value(1, 3);\n    return x;").unwrap();
        assert_eq!(m.tier, MatchTier::LineFuzzy);
        assert!(m.confidence >= 0.85);
        assert!(content[m.byte_range()].contains("compute_value"));
    }

    #[test]
    fn line_fuzzy_refuses_unrelated_text() {
        assert!(try_line_fuzzy(SAMPLE, "totally unrelated content here\nand more of it").is_none());
    }

    /// rustcoder computed offsets as `sum(line.len() + 1)`, which is wrong for
    /// CRLF files — the byte range would drift by one per preceding line.
    #[test]
    fn offsets_are_correct_with_crlf_line_endings() {
        let content = "alpha\r\nbeta\r\ngamma\r\n";
        let offsets = line_start_offsets(content);
        // "alpha\r\n" is 7 bytes, "beta\r\n" is 6 — so line 3 starts at 13.
        // Summing `line.len() + 1` would have said 5+1 + 4+1 = 11.
        assert_eq!(offsets[0], 0);
        assert_eq!(offsets[1], 7);
        assert_eq!(offsets[2], 13);
        let m = try_whitespace_normalized(content, "gamma").unwrap();
        assert!(content[m.byte_range()].starts_with("gamma"));
    }

    #[test]
    fn offsets_are_correct_without_trailing_newline() {
        let content = "alpha\nbeta";
        let m = try_whitespace_normalized(content, "beta").unwrap();
        assert_eq!(&content[m.byte_range()], "beta");
    }

    /// A short target keeps character granularity — that is the whole value of
    /// this tier, and the budget must not buy speed by giving it away.
    #[test]
    fn a_short_target_is_still_compared_character_by_character() {
        let short = "    let x = compute_value(1, 2);\n    return x;";
        assert_eq!(
            Granularity::for_sweep(short, 2_000, 4),
            Granularity::Chars,
            "a two-line target against a two-thousand-line file is the common case"
        );
    }

    /// A long target drops to lines. Character granularity there is the
    /// difference between a fifth of a second and a minute, and buys nothing —
    /// a forty-line block differing by one line scores 0.975 either way.
    #[test]
    fn a_long_target_drops_to_line_granularity_rather_than_stalling() {
        let long: String = (0..40)
            .map(|i| format!("    let absent_{i} = nowhere({i}, \"gone\");\n"))
            .collect();
        assert_eq!(Granularity::for_sweep(&long, 1_500, 4), Granularity::Lines);
    }

    /// **The property a user would notice.** A model pasting a block that does
    /// not match is routine, and a tool call has no cancellation checkpoint
    /// inside it — so an unbounded sweep is not a slow edit, it is an editor
    /// that stopped answering. Measured at 29.8 s before the budget existed.
    ///
    /// The bound looks generous, and is calibrated rather than guessed. Measured
    /// on this fixture: **0.12 s** release and **1.7 s** debug with the budget,
    /// against **29.7 s** release without it — so a debug regression lands in the
    /// minutes. Eight seconds is far enough above the honest debug cost to never
    /// flake on a loaded machine and still two orders of magnitude below the
    /// failure it guards. A tighter bound would buy no sensitivity and would
    /// eventually fail for reasons that have nothing to do with this code.
    #[test]
    fn a_failed_multi_line_match_against_a_large_file_returns_promptly() {
        let content: String = (0..1_500)
            .map(|i| format!("    let value_{i} = compute({i}, \"label\");\n"))
            .collect();
        let missing: String = (0..40)
            .map(|i| format!("    let absent_{i} = nowhere({i}, \"gone\");\n"))
            .collect();

        let started = std::time::Instant::now();
        assert!(find(&content, &missing).is_none(), "nothing should match");
        let elapsed = started.elapsed();

        assert!(
            elapsed < std::time::Duration::from_secs(8),
            "took {elapsed:?}; an unbounded character sweep took 29.7 s release and minutes in debug, and the write-review \
             hook runs this cascade a second time before the tool does"
        );
    }

    #[test]
    fn counts_exact_occurrences() {
        assert_eq!(count_exact("a b a b a", "a"), 3);
        assert_eq!(count_exact("abc", "zzz"), 0);
    }

    #[test]
    fn empty_old_text_never_matches() {
        assert!(find(SAMPLE, "").is_none());
    }
}
