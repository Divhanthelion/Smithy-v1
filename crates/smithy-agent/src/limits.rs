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
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_steps: 60,
            max_seconds: 900,
            context_warn: 32_000,
            context_hard: 110_000,
            max_parse_retries: 3,
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
    warned: bool,
    warned_steps: bool,
}

impl Budget {
    pub fn new(limits: Limits) -> Self {
        Budget {
            limits,
            started: Instant::now(),
            steps: 0,
            last_prompt_tokens: 0,
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
    fn stops_when_context_exceeds_the_hard_ceiling() {
        let mut b = Budget::new(limits());
        b.tick().unwrap();
        b.record_prompt_tokens(250);
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
}
