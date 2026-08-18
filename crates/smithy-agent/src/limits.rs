//! Resource ceilings.
//!
//! Ported from coda, whose spec called these "the cheapest, most important
//! reliability layer": they make a runaway loop impossible regardless of what
//! the model does. Step and time budgets are per user-turn; the context budget
//! is cumulative and read from the endpoint's own token accounting.

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use smithy_tools::{middle_truncate, GatePause};

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
    /// Soft cap on cumulative tool-result characters in one turn, and the
    /// hard cap on a *single* result.
    ///
    /// Per-call caps are fine in isolation (`read` 2000 lines, `web_fetch` 64k
    /// chars) but nothing bound their *sum*. A result over this size is middle-
    /// truncated before it enters History. Past the same threshold *cumulatively*
    /// we also append a narrowing hint. Do not scale this with a 1M window —
    /// eight percent of a million tokens is a cap that never fires.
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
/// Eight percent of the window, counted as characters (`chars ≈ tokens * 4`),
/// then clamped. The fraction is enough headroom for a few focused reads on a
/// 32k–128k window. A 1M window must not raise this to hundreds of thousands
/// of characters — that is how a 2000-line `read` sat in History forever.
pub fn tool_result_warn_for_window(context_length: i64) -> usize {
    const SHARE: f64 = 0.08;
    const CEILING: usize = 24_000;
    let scaled = ((context_length.max(0) as f64) * SHARE * 4.0) as usize;
    scaled.min(CEILING)
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
    /// Shared with tool-ctx hooks so Review wait is not counted as a loop.
    gate: GatePause,
    /// Actual tool calls this turn, including parallel ones in a step.
    tool_calls: usize,
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
        Self::with_gate(limits, last_prompt_tokens, GatePause::default())
    }

    /// Same as [`Self::seeded`], sharing a pause with the tool context so a
    /// Review wait does not burn the wall clock.
    pub fn with_gate(limits: Limits, last_prompt_tokens: i64, gate: GatePause) -> Self {
        Budget {
            limits,
            started: Instant::now(),
            steps: 0,
            last_prompt_tokens,
            tool_result_chars: 0,
            warned: false,
            warned_steps: false,
            gate,
            tool_calls: 0,
        }
    }

    fn elapsed_at(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.started)
            .saturating_sub(self.gate.paused_at(now))
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
        let used = self.tool_calls.max(self.steps);
        if self.warned_steps || used < self.wrap_up_step() {
            return None;
        }
        self.warned_steps = true;
        let left = self.limits.max_steps.saturating_sub(used);
        Some(format!(
            "You have used {used} of {} tool calls for this turn; about {left} remain. Start \
             finishing: verify what you have already changed, then reply with what is done and \
             what is still outstanding. Do not begin new work you cannot complete.",
            self.limits.max_steps
        ))
    }

    /// Call at the top of each loop iteration. `Err` means a ceiling was hit.
    pub fn tick(&mut self) -> Result<(), Stop> {
        self.tick_at(Instant::now())
    }

    fn tick_at(&mut self, now: Instant) -> Result<(), Stop> {
        self.steps += 1;
        if self.steps > self.limits.max_steps {
            return Err(Stop::Steps(self.limits.max_steps));
        }
        if self.tool_calls >= self.limits.max_steps {
            return Err(Stop::Steps(self.limits.max_steps));
        }
        if self.elapsed_at(now) >= Duration::from_secs(self.limits.max_seconds) {
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

    /// Remaining wall-clock budget, ignoring time spent waiting on the user.
    pub fn remaining(&self) -> Duration {
        let cap = Duration::from_secs(self.limits.max_seconds);
        cap.saturating_sub(self.elapsed_at(Instant::now()))
    }

    /// Count tools executed in this step, including parallel ones.
    pub fn record_tool_calls(&mut self, n: usize) {
        self.tool_calls = self.tool_calls.saturating_add(n);
    }

    pub fn tool_calls(&self) -> usize {
        self.tool_calls
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

    /// Count a tool result toward the per-turn aggregate. A single result over
    /// the cap is middle-truncated *before* it enters history. Past the cap
    /// cumulatively, a narrowing hint is appended. Append-only-safe: we shape
    /// the result, we do not rewrite it later.
    pub fn annotate_tool_result(&mut self, content: &mut String) {
        let cap = self.limits.tool_result_warn_chars;
        if cap > 0 && content.chars().count() > cap {
            *content = middle_truncate(content, cap);
            content.push_str(
                "\n\n[truncated; page with offset/limit or a narrower query rather than fetching more]",
            );
        }
        self.tool_result_chars = self.tool_result_chars.saturating_add(content.len());
        if self.tool_result_chars > cap {
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
        assert_eq!(Stop::Time(3600).to_string(), "time limit reached (3600s)");
        assert!(Stop::Context(120_000).to_string().contains("120000 tokens"));
    }

    #[test]
    fn stops_at_the_time_ceiling() {
        let mut b = Budget::new(Limits {
            max_seconds: 10,
            ..limits()
        });
        let later = Instant::now() + Duration::from_secs(11);
        assert_eq!(b.tick_at(later), Err(Stop::Time(10)));
    }

    /// A human reading a diff is not a runaway loop. The wall clock has to
    /// ignore the wait or walking away from Review kills the turn.
    #[test]
    fn waiting_on_the_user_does_not_count_against_the_clock() {
        let gate = GatePause::default();
        let mut b = Budget::with_gate(
            Limits {
                max_seconds: 10,
                ..limits()
            },
            0,
            gate.clone(),
        );
        let t0 = Instant::now();
        let _hold = gate.hold();
        let later = t0 + Duration::from_secs(30);
        assert_eq!(
            b.tick_at(later),
            Ok(()),
            "Review wait must not burn the turn clock"
        );
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
        assert!(second.contains("running long"), "and say why: {second}");
        // A small result is still intact — we truncate only when *this* body
        // is over the cap.
        assert!(second.starts_with(&"y".repeat(50)));
        assert_eq!(b.tool_result_chars(), 110);
    }

    #[test]
    fn a_single_oversized_result_is_truncated_before_it_enters_history() {
        let mut b = Budget::new(Limits {
            tool_result_warn_chars: 80,
            ..limits()
        });
        let mut body = "HEAD".to_string() + &"x".repeat(400) + "TAIL";
        b.annotate_tool_result(&mut body);
        assert!(body.contains("HEAD"), "{body}");
        assert!(body.contains("TAIL"), "{body}");
        assert!(body.contains("truncated"), "{body}");
        assert!(
            body.chars().count() < 400,
            "the dump must not land whole: {}",
            body.chars().count()
        );
        assert!(!body.contains(&"x".repeat(200)), "the middle is what goes");
    }

    #[test]
    fn tool_result_warn_does_not_scale_with_a_million_token_window() {
        let small = tool_result_warn_for_window(32_768);
        let large = tool_result_warn_for_window(1_000_000);
        assert_eq!(small, ((32_768.0_f64) * 0.08 * 4.0) as usize);
        assert_eq!(large, 24_000, "a 1M window must not raise the sludge cap");
        assert!(large >= small);
    }

    #[test]
    fn the_step_warning_counts_tool_calls_not_just_loop_iterations() {
        let mut b = Budget::new(Limits {
            max_steps: 10,
            ..limits()
        });
        b.tick().ok();
        b.record_tool_calls(8);
        let warning = b.should_warn_steps().expect("warned");
        assert!(
            warning.contains("used 8 of 10 tool calls"),
            "the counted unit is what the message must name: {warning}"
        );
    }

    #[test]
    fn remaining_budget_shrinks_with_elapsed_time() {
        let b = Budget::new(Limits {
            max_seconds: 10,
            ..limits()
        });
        let left = b.remaining();
        assert!(left <= Duration::from_secs(10));
        assert!(
            left > Duration::from_secs(8),
            "a fresh budget still has nearly all of it: {left:?}"
        );
    }
}
