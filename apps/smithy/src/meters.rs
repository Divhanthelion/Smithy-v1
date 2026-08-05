//! What the machine and the account are being spent.
//!
//! Two readouts in the top-right of the menu bar, filled from here because
//! `smithy-editor` renders strings and does not know what a token costs or how
//! to read a process table.
//!
//! ## Memory
//!
//! Smithy's own resident set, plus the `rust-analyzer` **we** spawned, and —
//! separately — every other `rust-analyzer` on the machine. The language server
//! is the interesting number: a single instance on a large workspace was
//! measured at **5.12 GB**. Other editors' analyzers used to be summed into the
//! same figure, which made Smithy look responsible for nine gigabytes that
//! belonged to Claude Code. Ours and theirs are named apart now.
//! `SMITHY_LSP_LIGHT=1` exists for the case that *is* ours, and nobody would
//! know to set it.
//!
//! Sampled rather than watched: reading the process table is cheap but not free,
//! and a meter that updated faster than you can read it would only cost battery.
//!
//! ## Spend
//!
//! Session cost from the endpoint's own `usage` accounting multiplied by the
//! model's list price, and — for DeepSeek, the one provider here that offers it
//! — the balance actually left on the account.
//!
//! The two answer different questions. Session cost is "what did *this*
//! conversation cost", which is the one that teaches you something: a long
//! context re-sends its whole prefix on every request, so the same question
//! costs more at turn forty than at turn four. Balance is "how much runway is
//! left", which is what you want when you have put ten dollars on an account.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use smithy_agent::session::Usage;

/// Refresh interval for the memory sample.
pub const MEMORY_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

/// How often the account balance is re-fetched.
///
/// Far slower than the memory sample: it is a network round trip on someone
/// else's rate limit, and a balance does not move between turns.
pub const BALANCE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(180);

/// Warn when a single process passes this. Chosen from the observed 5.12 GB
/// rust-analyzer: high enough not to cry wolf on a healthy one, low enough to
/// notice before the machine starts swapping.
const MEMORY_WARN_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// Warn when the account balance falls below this, in whatever currency it is
/// denominated. A round number rather than a percentage — you top up in
/// absolute amounts, so runway is what matters.
const BALANCE_WARN: f64 = 1.0;

/// One sample of process memory.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MemorySample {
    /// Smithy's own resident set.
    pub own_bytes: u64,
    /// `rust-analyzer` processes descended from this smithy.
    pub analyzer_bytes: u64,
    pub analyzer_count: usize,
    /// Everyone else's `rust-analyzer` — Cursor, Claude Code, VS Code, a
    /// previous smithy that did not exit cleanly.
    pub other_analyzer_bytes: u64,
    pub other_analyzer_count: usize,
}

/// Last usage read successfully from one live session.
///
/// The session mutex is intentionally contended for the duration of a turn.
/// Keeping this side cache prevents the menu meter from flashing back to zero
/// precisely while the provider is doing billable work.
#[derive(Clone, Default)]
pub struct UsageCache {
    inner: Arc<std::sync::Mutex<(Option<usize>, Usage)>>,
}

impl UsageCache {
    /// Install the accounting snapshot before the session can be claimed by a
    /// turn. The first meter tick may then miss `try_lock` without erasing the
    /// restored spend.
    pub fn seed(
        &self,
        session: &Arc<tokio::sync::Mutex<smithy_agent::Session>>,
        usage: Usage,
    ) {
        self.seed_identity(Arc::as_ptr(session) as usize, usage);
    }

    fn seed_identity(&self, identity: usize, usage: Usage) {
        let mut cached = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        *cached = (Some(identity), usage);
    }

    fn load_for(
        &self,
        session: Option<&Arc<tokio::sync::Mutex<smithy_agent::Session>>>,
    ) -> Usage {
        let Some(session) = session else {
            return self.update(None, None);
        };
        let identity = Arc::as_ptr(session) as usize;
        let observed = session.try_lock().ok().map(|guard| guard.usage());
        self.update(Some(identity), observed)
    }

    fn update(&self, identity: Option<usize>, observed: Option<Usage>) -> Usage {
        let mut cached = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        if cached.0 != identity {
            *cached = (identity, Usage::default());
        }
        if let Some(usage) = observed {
            cached.1 = usage;
        }
        cached.1
    }
}

impl MemorySample {
    /// The menu-bar string, or empty when there is nothing worth saying.
    pub fn render(&self) -> String {
        if self.own_bytes == 0
            && self.analyzer_bytes == 0
            && self.other_analyzer_bytes == 0
        {
            return String::new();
        }
        let mut parts = Vec::new();
        if self.own_bytes > 0 {
            parts.push(format!("smithy {}", human_bytes(self.own_bytes)));
        }
        if self.analyzer_count > 0 {
            let label = if self.analyzer_count == 1 {
                "rust-analyzer".to_string()
            } else {
                format!("rust-analyzer ×{}", self.analyzer_count)
            };
            parts.push(format!("{label} {}", human_bytes(self.analyzer_bytes)));
        }
        if self.other_analyzer_count > 0 {
            parts.push(format!(
                "+{} elsewhere",
                human_bytes(self.other_analyzer_bytes)
            ));
        }
        parts.join(" · ")
    }

    /// Whether anything here is worth colouring.
    ///
    /// Only *our* analyzer and our own RSS trip the warn — someone else's
    /// nine-gigabyte instance is worth naming, not blaming on this window.
    pub fn is_heavy(&self) -> bool {
        self.analyzer_bytes >= MEMORY_WARN_BYTES || self.own_bytes >= MEMORY_WARN_BYTES
    }
}

/// Read the process table once.
///
/// `refresh_processes` rather than `new_all`: the full constructor also collects
/// disks, networks and components, none of which are wanted here and all of
/// which cost.
pub fn sample_memory() -> MemorySample {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing().with_memory(),
    );

    let own_pid = sysinfo::get_current_pid().ok();
    let mut sample = MemorySample::default();

    // Parent map so we can tell a rust-analyzer we spawned from one Cursor or
    // Claude Code is keeping around. Walk is bounded; process trees here are
    // shallow.
    let parents: HashMap<Pid, Option<Pid>> = system
        .processes()
        .iter()
        .map(|(pid, process)| (*pid, process.parent()))
        .collect();

    let descended_from_us = |pid: Pid| -> bool {
        let Some(own) = own_pid else {
            return false;
        };
        let mut current = Some(pid);
        for _ in 0..16 {
            match current {
                Some(p) if p == own => return true,
                Some(p) => current = parents.get(&p).copied().flatten(),
                None => return false,
            }
        }
        false
    };

    for (pid, process) in system.processes() {
        let memory = process.memory();
        if Some(*pid) == own_pid {
            sample.own_bytes = memory;
            continue;
        }
        // Match on the executable name, not the full command line: the analyzer
        // is spawned by us and by any editor the user has open.
        let name = process.name().to_string_lossy();
        if name.contains("rust-analyzer") && !name.contains("proc-macro") {
            if descended_from_us(*pid) {
                sample.analyzer_bytes += memory;
                sample.analyzer_count += 1;
            } else {
                sample.other_analyzer_bytes += memory;
                sample.other_analyzer_count += 1;
            }
        }
    }
    sample
}

/// What a session has cost, and what is left.
#[derive(Debug, Clone, Default)]
pub struct Spend {
    pub usage: Usage,
    /// `None` when the model's price is not known — an unlisted OpenRouter model,
    /// or a local server where the question does not apply.
    pub session_cost: Option<f64>,
    /// `None` for providers with no balance endpoint.
    pub balance: Option<smithy_agent::catalogue::Balance>,
}

impl Spend {
    pub fn render(&self) -> String {
        let mut parts = Vec::new();
        match self.session_cost {
            // Sub-cent costs are the common case early in a session and `$0.00`
            // reads as "free" rather than "not much yet".
            Some(cost) if cost > 0.0 && cost < 0.01 => parts.push("<$0.01".to_string()),
            Some(cost) if cost > 0.0 => parts.push(format!("${cost:.2}")),
            // No price known: tokens are still worth showing, since they are what
            // the cost is proportional to.
            None if self.usage.total_tokens() > 0 => {
                parts.push(format!("{} tok", format_tokens(self.usage.total_tokens())))
            }
            _ => {}
        }
        if let Some(balance) = &self.balance {
            parts.push(format!("{} left", balance.render()));
        }
        parts.join(" · ")
    }

    pub fn is_low(&self) -> bool {
        self.balance
            .as_ref()
            .map(|b| !b.available || b.total < BALANCE_WARN)
            .unwrap_or(false)
    }
}

/// A balance shared between the fetcher and the UI.
///
/// Stored as cents in an atomic rather than behind a lock: it is written by a
/// worker and read by the UI thread on every meter tick, and a lock held across
/// either would be the only contended thing in the app.
#[derive(Clone, Default)]
pub struct BalanceCache {
    cents: Arc<AtomicU64>,
    /// Currency and availability change so rarely that a second atomic is not
    /// worth it; `u64::MAX` in `cents` means "nothing fetched yet".
    currency: Arc<std::sync::Mutex<String>>,
    available: Arc<std::sync::atomic::AtomicBool>,
}

impl BalanceCache {
    pub fn new() -> Self {
        Self {
            cents: Arc::new(AtomicU64::new(u64::MAX)),
            currency: Arc::new(std::sync::Mutex::new(String::new())),
            available: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        }
    }

    pub fn store(&self, balance: &smithy_agent::catalogue::Balance) {
        self.cents
            .store((balance.total * 100.0).round().max(0.0) as u64, Ordering::Relaxed);
        if let Ok(mut currency) = self.currency.lock() {
            *currency = balance.currency.clone();
        }
        self.available.store(balance.available, Ordering::Relaxed);
    }

    pub fn load(&self) -> Option<smithy_agent::catalogue::Balance> {
        let cents = self.cents.load(Ordering::Relaxed);
        if cents == u64::MAX {
            return None;
        }
        Some(smithy_agent::catalogue::Balance {
            currency: self.currency.lock().ok()?.clone(),
            total: cents as f64 / 100.0,
            available: self.available.load(Ordering::Relaxed),
        })
    }

    /// Forget the cached figure — after a provider switch, where the old
    /// account's balance would be actively misleading.
    pub fn clear(&self) {
        self.cents.store(u64::MAX, Ordering::Relaxed);
    }
}

/// Assemble the spend readout from what is already known.
///
/// Cheap enough to run on every meter tick: it reads a small settings file, two
/// atomics and the session's own counters. No network, no keychain — the balance
/// arrives asynchronously via [`spawn_balance_poller`] and is only read here.
///
/// `try_lock` on the session, never `lock`: the session's mutex is held for the
/// *entire* duration of a turn, so blocking on it would freeze the UI thread for
/// as long as the model takes to answer. A tick that finds it busy simply keeps
/// the previous figure and tries again in five seconds.
pub fn spend_now(
    data_dir: &std::path::Path,
    model_label: &str,
    session: Option<&Arc<tokio::sync::Mutex<smithy_agent::Session>>>,
    balance: &BalanceCache,
    usage_cache: &UsageCache,
) -> Spend {
    let usage = usage_cache.load_for(session);

    let config = smithy_agent::AgentConfig::load(data_dir);
    let model = config.active().model.clone();
    // The label carries the model the session actually connected with, which is
    // the authority when settings have been changed but not reconnected.
    let model = if model_label.starts_with(&model) || model.is_empty() {
        model
    } else {
        model_label.split(' ').next().unwrap_or(&model).to_string()
    };

    let session_cost = price_of(config.provider, &model).map(|(prompt, completion, cached)| {
        usage.cost(prompt, completion, cached)
    });

    Spend {
        usage,
        session_cost,
        balance: balance.load(),
    }
}

/// List price per million tokens, when it is known.
///
/// Only DeepSeek is answerable offline, from the table in its provider module.
/// OpenRouter's prices are per-model and live, and fetching the whole catalogue
/// on a five-second tick to price one model would be absurd — so an OpenRouter
/// session shows tokens rather than a number that might be wrong.
fn price_of(
    provider: smithy_agent::ProviderChoice,
    model: &str,
) -> Option<(f64, f64, f64)> {
    match provider {
        smithy_agent::ProviderChoice::DeepSeek => {
            let (prompt, completion) = smithy_agent::providers::deepseek::pricing_for(model)?;
            // DeepSeek's published cache-hit rate is a tenth of the cold
            // prompt rate. Pricing cache hits at the cold rate was why the
            // meter made the architecture's best feature look expensive.
            Some((prompt, completion, prompt * 0.1))
        }
        // A local server bills nothing, so a cost of zero would be true but
        // uninformative; tokens are the useful figure.
        smithy_agent::ProviderChoice::LmStudio => None,
        smithy_agent::ProviderChoice::OpenRouter => None,
    }
}

/// Re-fetch the account balance in the background, forever.
///
/// Its own task rather than the meter effect, because it is a network call on
/// someone else's rate limit and must not run at UI cadence. Silent on failure:
/// a balance that cannot be fetched shows nothing, which is the honest rendering
/// of "unknown" and better than an error in the menu bar.
pub fn spawn_balance_poller(agent: crate::app_state::AgentState, data_dir: std::path::PathBuf) {
    let cache = agent.balance.clone();
    crate::runtime::tokio_runtime().spawn(async move {
        loop {
            let config = smithy_agent::AgentConfig::load(&data_dir);
            if config.provider == smithy_agent::ProviderChoice::DeepSeek {
                let key = tokio::task::spawn_blocking(|| {
                    smithy_agent::ProviderChoice::DeepSeek.api_key()
                })
                .await
                .ok()
                .flatten();

                if let Some(key) = key {
                    if let Ok(balance) = smithy_agent::catalogue::deepseek_balance(
                        &config.deepseek.base_url,
                        &key,
                    )
                    .await
                    {
                        cache.store(&balance);
                    }
                    // Fetch failures leave the honest rendering, "unknown".
                }
            } else {
                // A stale figure from another account is worse than none.
                cache.clear();
            }
            tokio::time::sleep(BALANCE_INTERVAL).await;
        }
    });
}

fn human_bytes(bytes: u64) -> String {
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.1} GB", b / GB)
    } else {
        format!("{:.0} MB", b / MB)
    }
}

fn format_tokens(tokens: i64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1e6)
    } else if tokens >= 1_000 {
        format!("{:.0}k", tokens as f64 / 1e3)
    } else {
        tokens.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smithy_agent::catalogue::Balance;

    #[test]
    fn memory_reads_the_way_a_person_would_say_it() {
        let sample = MemorySample {
            own_bytes: 536 * 1024 * 1024,
            analyzer_bytes: 5_497_558_138,
            analyzer_count: 1,
            ..Default::default()
        };
        assert_eq!(sample.render(), "smithy 536 MB · rust-analyzer 5.1 GB");
    }

    /// Several analyzers is the case worth naming — it is how you end up with
    /// six gigabytes without noticing.
    #[test]
    fn several_analyzers_are_counted_and_labelled() {
        let sample = MemorySample {
            own_bytes: 100 * 1024 * 1024,
            analyzer_bytes: 6 * 1024 * 1024 * 1024,
            analyzer_count: 3,
            ..Default::default()
        };
        assert!(sample.render().contains("rust-analyzer ×3"), "{}", sample.render());
        assert!(sample.is_heavy());
    }

    /// Someone else's nine-gigabyte instance must not look like ours.
    #[test]
    fn other_editors_analyzers_are_named_apart() {
        let sample = MemorySample {
            own_bytes: 137 * 1024 * 1024,
            analyzer_bytes: 205 * 1024 * 1024,
            analyzer_count: 1,
            other_analyzer_bytes: 9 * 1024 * 1024 * 1024,
            other_analyzer_count: 1,
        };
        assert_eq!(
            sample.render(),
            "smithy 137 MB · rust-analyzer 205 MB · +9.0 GB elsewhere"
        );
        assert!(!sample.is_heavy());
    }

    #[test]
    fn a_quiet_machine_does_not_warn() {
        let sample = MemorySample {
            own_bytes: 300 * 1024 * 1024,
            analyzer_bytes: 800 * 1024 * 1024,
            analyzer_count: 1,
            ..Default::default()
        };
        assert!(!sample.is_heavy());
    }

    #[test]
    fn nothing_measured_renders_nothing_rather_than_zeroes() {
        assert_eq!(MemorySample::default().render(), "");
    }

    // --- spend ---

    fn balance(total: f64) -> Balance {
        Balance {
            currency: "USD".into(),
            total,
            available: true,
        }
    }

    #[test]
    fn a_session_cost_and_a_balance_read_together() {
        let spend = Spend {
            usage: Usage {
                prompt_tokens: 500_000,
                completion_tokens: 100_000,
                cached_tokens: 0,
                reasoning_tokens: 0,
                requests: 12,
            },
            session_cost: Some(0.098),
            balance: Some(balance(9.93)),
        };
        assert_eq!(spend.render(), "$0.10 · $9.93 left");
    }

    /// `$0.00` reads as free. Early in a session the true answer is "not much
    /// yet", and those are different claims.
    #[test]
    fn a_sub_cent_cost_says_so_rather_than_rounding_to_zero() {
        let spend = Spend {
            session_cost: Some(0.0021),
            ..Default::default()
        };
        assert_eq!(spend.render(), "<$0.01");
    }

    /// A local model, or one whose price we do not know, still has a token
    /// count worth seeing.
    #[test]
    fn an_unpriced_model_falls_back_to_tokens() {
        let spend = Spend {
            usage: Usage {
                prompt_tokens: 45_000,
                completion_tokens: 5_000,
                cached_tokens: 0,
                reasoning_tokens: 0,
                requests: 3,
            },
            session_cost: None,
            balance: None,
        };
        assert_eq!(spend.render(), "50k tok");
    }

    #[test]
    fn nothing_spent_renders_nothing() {
        assert_eq!(Spend::default().render(), "");
    }

    #[test]
    fn a_low_or_unusable_balance_warns() {
        assert!(Spend {
            balance: Some(balance(0.42)),
            ..Default::default()
        }
        .is_low());
        assert!(!Spend {
            balance: Some(balance(9.93)),
            ..Default::default()
        }
        .is_low());
        assert!(
            Spend {
                balance: Some(Balance {
                    available: false,
                    ..balance(50.0)
                }),
                ..Default::default()
            }
            .is_low(),
            "a suspended account is low however much is on it"
        );
    }

    // --- the cache ---

    #[test]
    fn an_unfetched_balance_is_none_rather_than_zero() {
        assert!(BalanceCache::new().load().is_none());
    }

    #[test]
    fn a_stored_balance_round_trips_to_the_cent() {
        let cache = BalanceCache::new();
        cache.store(&balance(9.93));
        let loaded = cache.load().expect("stored");
        assert!((loaded.total - 9.93).abs() < 1e-9);
        assert_eq!(loaded.currency, "USD");
    }

    /// After a provider switch the previous account's balance is worse than no
    /// balance at all.
    #[test]
    fn clearing_forgets_the_previous_accounts_figure() {
        let cache = BalanceCache::new();
        cache.store(&balance(9.93));
        cache.clear();
        assert!(cache.load().is_none());
    }

    /// The session lock is held for the whole provider turn. Treating a missed
    /// `try_lock` as zero made the spend meter erase its last true value exactly
    /// while another request was accruing cost.
    #[test]
    fn a_contended_session_keeps_the_last_successful_usage_sample() {
        let cache = UsageCache::default();
        let measured = Usage {
            prompt_tokens: 12_000,
            completion_tokens: 800,
            cached_tokens: 7_000,
            reasoning_tokens: 300,
            requests: 2,
        };
        assert_eq!(cache.update(Some(7), Some(measured)), measured);
        assert_eq!(
            cache.update(Some(7), None),
            measured,
            "lock contention reset the usage meter"
        );
        assert_eq!(
            cache.update(Some(8), None),
            Usage::default(),
            "a different session inherited the previous conversation's spend"
        );
    }

    /// A resumed session can be installed and claimed by an immediate send
    /// before the meter's first tick. Seeding at installation must make that
    /// first contended observation show restored spend, not zero or the prior
    /// conversation's value.
    #[test]
    fn a_newly_installed_contended_session_starts_from_restored_usage() {
        let cache = UsageCache::default();
        let old = Usage {
            prompt_tokens: 100,
            requests: 1,
            ..Default::default()
        };
        let restored = Usage {
            prompt_tokens: 42_000,
            completion_tokens: 900,
            cached_tokens: 30_000,
            reasoning_tokens: 400,
            requests: 8,
        };
        cache.seed_identity(1, old);
        cache.seed_identity(2, restored);
        assert_eq!(
            cache.update(Some(2), None),
            restored,
            "the first missed lock discarded restored accounting"
        );
    }

    #[test]
    fn the_cost_calculation_is_per_million_tokens() {
        let usage = Usage {
            prompt_tokens: 1_000_000,
            completion_tokens: 1_000_000,
            cached_tokens: 0,
            reasoning_tokens: 0,
            requests: 1,
        };
        // DeepSeek v4-flash list prices; no cache hits.
        assert!((usage.cost(0.14, 0.28, 0.014) - 0.42).abs() < 1e-9);
    }

    #[test]
    fn cached_tokens_are_priced_below_cold_prompt_tokens() {
        let usage = Usage {
            prompt_tokens: 1_000_000,
            completion_tokens: 0,
            cached_tokens: 900_000,
            reasoning_tokens: 0,
            requests: 1,
        };
        // 100k cold @ 0.14 + 900k cached @ 0.014 = 0.014 + 0.0126 = 0.0266
        assert!((usage.cost(0.14, 0.28, 0.014) - 0.0266).abs() < 1e-9);
        assert!((usage.cache_hit_rate().unwrap() - 0.9).abs() < 1e-9);
    }
}
