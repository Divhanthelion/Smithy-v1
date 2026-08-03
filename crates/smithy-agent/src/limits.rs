//! Resource ceilings.
//!
//! Ported from coda, whose spec called these "the cheapest, most important
//! reliability layer": they make a runaway loop impossible regardless of what
//! the model does. Step and time budgets are per user-turn; the context budget
//! is cumulative and read from the endpoint's own token accounting.

use std::time::Instant;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Limits {
    pub max_steps: usize,
    pub max_seconds: u64,
    /// Warn once when a single prompt crosses this.
    pub context_warn: i64,
    /// Hard stop when prompt tokens cross this.
    ///
    /// Note on provenance: coda's value sat just under its endpoint's KV ceiling
    /// on the theory that accuracy held up to that point. Its own post-mortem
    /// retracted that — the benchmark that "proved" it had no negative control
    /// and was scoring its own reasoning text. What *is* well supported is that
    /// cold prefill time grows superlinearly with context. So treat this as a
    /// latency ceiling, which is measured, rather than an accuracy one, which
    /// is not.
    pub context_hard: i64,
    /// Give up after this many consecutive unusable responses.
    pub max_parse_retries: usize,
    /// Soft cap on cumulative tool-result characters in one turn.
    ///
    /// Per-call caps are fine in isolation (`read` 2000 lines, `web_fetch` 64k
    /// chars) but nothing bound their *sum*. One `web_fetch` at default is
    /// ~16k tokens of uncached history; three greps and two fetches clear the
    /// soft context warn inside five steps — all permanent, none of it in the
    /// cached prefix (HANDOFF §5.1). Past this threshold we append a narrowing
    /// hint to the result itself: warn, don't truncate — cutting risks removing
    /// the answer the model needed, and would fail silently.
    #[serde(default = "default_tool_result_warn_chars")]
    pub tool_result_warn_chars: usize,
}

fn default_tool_result_warn_chars() -> usize {
    // 8% of the default hard ceiling, as chars (×4). Same formula
    // `suggested_limits` uses once a window is known.
    tool_result_warn_for_window(110_000)
}

/// Soft tool-result budget derived from the model's context window.
///
/// Eight percent of the window, counted as characters (`chars ≈ tokens * 4`).
/// The fraction is the failure above: enough headroom for a few focused reads,
/// tight enough that a default `web_fetch` (64k chars) trips the warn on a
/// 110k-class window before a second one lands.
pub fn tool_result_warn_for_window(context_length: i64) -> usize {
    const SHARE: f64 = 0.08;
    ((context_length.max(0) as f64) * SHARE * 4.0) as usize
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_steps: 60,
            max_seconds: 900,
            context_warn: 32_000,
            context_hard: 110_000,
            max_parse_retries: 3,
            tool_result_warn_chars: default_tool_result_warn_chars(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Stop {
    Steps(usize),
    Time(u64),
    Context(i64),
}

impl std::fmt::Display for Stop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Stop::Steps(n) => write!(f, "step limit reached ({n})"),
            Stop::Time(s) => write!(f, "time limit reached ({s}s)"),
            Stop::Context(t) => write!(f, "context ceiling reached ({t} tokens)"),
        }
    }
}

/// Per-turn budget tracker.
pub struct Budget {
    limits: Limits,
    started: Instant,
    steps: usize,
    last_prompt_tokens: i64,
    /// Cumulative tool-result characters this turn (pre-annotation).
    tool_result_chars: usize,
    warned: bool,
    warned_steps: bool,
}

impl Budget {
    pub fn new(limits: Limits) -> Self {
        Self::seeded(limits, 0)
    }

    /// Start a turn already knowing the previous request's prompt size.
    ///
    /// Without this, `last_prompt_tokens` resets to 0 every turn and the
    /// context ceiling can only fire *after* a doomed first call has already
    /// been billed — exactly the failure at 130k against a 110k hard stop.
    pub fn seeded(limits: Limits, last_prompt_tokens: i64) -> Self {
        Budget {
            limits,
            started: Instant::now(),
            steps: 0,
            last_prompt_tokens,
            tool_result_chars: 0,
            warned: false,
            warned_steps: false,
        }
    }

    /// The step at which the model is told to start wrapping up.
    ///
    /// Four-fifths of the way through. Late enough not to cut work short,
    /// early enough that there is room to finish a file and say what is left.
    fn wrap_up_step(&self) -> usize {
        (self.limits.max_steps * 4) / 5
    }

    /// Whether this tick is the one that should carry a wrap-up warning.
    ///
    /// Fires once. A turn that died at the step ceiling used to do so with no
    /// warning at all — the observed session stopped mid-implementation having
    /// never been told it was running out, so it neither finished nor reported
    /// what remained.
    pub fn should_warn_steps(&mut self) -> Option<String> {
        if self.warned_steps || self.steps < self.wrap_up_step() {
            return None;
        }
        self.warned_steps = true;
        let left = self.limits.max_steps.saturating_sub(self.steps);
        Some(format!(
            "You have used {} of {} tool calls for this turn; about {left} remain. Start \
             finishing: verify what you have already changed, then reply with what is done and \
             what is still outstanding. Do not begin new work you cannot complete.",
            self.steps, self.limits.max_steps
        ))
    }

    /// Call at the top of each loop iteration. `Err` means a ceiling was hit.
    pub fn tick(&mut self) -> Result<(), Stop> {
        self.steps += 1;
        if self.steps > self.limits.max_steps {
            return Err(Stop::Steps(self.limits.max_steps));
        }
        if self.started.elapsed().as_secs() > self.limits.max_seconds {
            return Err(Stop::Time(self.limits.max_seconds));
        }
        if self.last_prompt_tokens > self.limits.context_hard {
            return Err(Stop::Context(self.last_prompt_tokens));
        }
        Ok(())
    }

    pub fn step(&self) -> usize {
        self.steps
    }

    /// Record the prompt token count from the last completion. Returns a warning
    /// the first time the soft ceiling is crossed, and only the first time —
    /// repeating it every step would train the user to ignore it.
    pub fn record_prompt_tokens(&mut self, tokens: i64) -> Option<String> {
        self.last_prompt_tokens = tokens;
        if !self.warned && tokens > self.limits.context_warn {
            self.warned = true;
            return Some(format!(
                "context is at {tokens} tokens (warn threshold {}); prefill latency grows \
                 sharply from here",
                self.limits.context_warn
            ));
        }
        None
    }

    pub fn last_prompt_tokens(&self) -> i64 {
        self.last_prompt_tokens
    }

    /// Count a tool result toward the per-turn aggregate and, past the soft
    /// threshold, append a narrowing hint to the content *before* it enters
    /// history. Append-only-safe: we shape the result, we do not rewrite it
    /// later. Warn rather than truncate — cutting risks deleting the answer.
    pub fn annotate_tool_result(&mut self, content: &mut String) {
        self.tool_result_chars = self.tool_result_chars.saturating_add(content.len());
        if self.tool_result_chars > self.limits.tool_result_warn_chars {
            content.push_str(
                "\n\n[results are running long this turn; narrow the query rather than fetching more]",
            );
        }
    }

    pub fn tool_result_chars(&self) -> usize {
        self.tool_result_chars
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> Limits {
        Limits {
            max_steps: 3,
            max_seconds: 900,
            context_warn: 100,
            context_hard: 200,
            max_parse_retries: 3,
            tool_result_warn_chars: 1_000,
        }
    }

    /// The turn that prompted this died at its step ceiling with no warning at
    /// all — it neither finished nor said what was left. The nudge has to come
    /// while there is still room to act on it.
    #[test]
    fn the_model_is_warned_before_the_step_ceiling_not_at_it() {
        let mut b = Budget::new(Limits {
            max_steps: 10,
            ..limits()
        });
        let mut warned_at = None;
        for step in 1..=10 {
            b.tick().expect("within budget");
            if b.should_warn_steps().is_some() && warned_at.is_none() {
                warned_at = Some(step);
            }
        }
        assert_eq!(warned_at, Some(8), "four-fifths of the way through");
    }

    /// Once only. A nudge repeated every step is noise the model learns to skip,
    /// and it would be appended to history each time.
    #[test]
    fn the_step_warning_fires_exactly_once() {
        let mut b = Budget::new(Limits {
            max_steps: 5,
            ..limits()
        });
        let mut warnings = 0;
        for _ in 0..5 {
            b.tick().ok();
            if b.should_warn_steps().is_some() {
                warnings += 1;
            }
        }
        assert_eq!(warnings, 1);
    }

    /// It has to say what to do, not merely that time is short.
    #[test]
    fn the_step_warning_asks_for_a_wrap_up_and_a_status() {
        let mut b = Budget::new(Limits {
            max_steps: 10,
            ..limits()
        });
        for _ in 0..8 {
            b.tick().ok();
        }
        let warning = b.should_warn_steps().expect("warned");
        assert!(warning.contains("remain"), "{warning}");
        assert!(warning.contains("outstanding"), "{warning}");
        assert!(
            warning.contains("Do not begin new work"),
            "starting something unfinishable is the failure being prevented: {warning}"
        );
    }

    #[test]
    fn stops_at_the_step_ceiling() {
        let mut b = Budget::new(limits());
        assert!(b.tick().is_ok());
        assert!(b.tick().is_ok());
        assert!(b.tick().is_ok());
        assert_eq!(b.tick(), Err(Stop::Steps(3)));
    }

    #[test]
    fn seeded_budget_stops_before_the_first_tick_completes_a_doomed_turn() {
        let mut b = Budget::seeded(
            Limits {
                context_hard: 100,
                ..limits()
            },
            250,
        );
        assert_eq!(b.tick(), Err(Stop::Context(250)));
    }

    #[test]
    fn context_under_the_ceiling_keeps_going() {
        let mut b = Budget::new(limits());
        b.tick().unwrap();
        b.record_prompt_tokens(150);
        assert!(b.tick().is_ok());
    }

    #[test]
    fn warns_once_and_only_once() {
        let mut b = Budget::new(limits());
        assert!(b.record_prompt_tokens(150).is_some());
        assert!(b.record_prompt_tokens(160).is_none());
        assert!(b.record_prompt_tokens(190).is_none());
    }

    #[test]
    fn does_not_warn_below_the_threshold() {
        let mut b = Budget::new(limits());
        assert!(b.record_prompt_tokens(50).is_none());
    }

    #[test]
    fn stop_reasons_are_legible() {
        assert_eq!(Stop::Steps(60).to_string(), "step limit reached (60)");
        assert!(Stop::Context(120_000).to_string().contains("120000 tokens"));
    }

    /// One web_fetch at default is ~64k chars. Without an aggregate cap those
    /// land uncached in history forever; the warn must fire on the result
    /// itself so the model narrows before the next call.
    #[test]
    fn tool_results_past_the_aggregate_cap_get_a_narrowing_hint() {
        let mut b = Budget::new(Limits {
            tool_result_warn_chars: 100,
            ..limits()
        });
        let mut first = "x".repeat(60);
        b.annotate_tool_result(&mut first);
        assert!(
            !first.contains("running long"),
            "under the cap must stay clean: {first}"
        );
        let mut second = "y".repeat(50);
        b.annotate_tool_result(&mut second);
        assert!(
            second.contains("narrow the query"),
            "over the cap must tell the model what to do: {second}"
        );
        assert!(
            second.contains("running long"),
            "and say why: {second}"
        );
        // The body is still intact — we warn, we do not truncate.
        assert!(second.starts_with(&"y".repeat(50)));
        assert_eq!(b.tool_result_chars(), 110);
    }

    #[test]
    fn tool_result_warn_scales_with_the_window() {
        let small = tool_result_warn_for_window(32_768);
        let large = tool_result_warn_for_window(1_000_000);
        assert!(large > small);
        assert_eq!(small, ((32_768.0_f64) * 0.08 * 4.0) as usize);
    }
}
