//! The tokio runtime the agent, the tools and the language server share.
//!
//! One runtime for the process, sized from the core count and overridable with
//! `SMITHY_WORKER_THREADS` — deliberately modest, because the machine is also
//! expected to be serving a model.

use std::sync::OnceLock;

/// Environment variable for configuring worker threads
const WORKER_THREADS_ENV: &str = "SMITHY_WORKER_THREADS";

/// Default number of worker threads
const DEFAULT_WORKER_THREADS: usize = 2;

/// Minimum number of worker threads
const MIN_WORKER_THREADS: usize = 1;

/// Maximum number of worker threads
const MAX_WORKER_THREADS: usize = 32;

/// Global tokio runtime for async operations (created once, reused)
pub fn tokio_runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        let worker_threads = get_worker_threads();

        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(worker_threads)
            .thread_name("smithy-runtime")
            .enable_all()
            .build()
            .expect("Failed to create tokio runtime")
    })
}

/// Get the configured number of worker threads
///
/// Priority:
/// 1. SMITHY_WORKER_THREADS environment variable
/// 2. Number of CPU cores (clamped to min/max)
/// 3. DEFAULT_WORKER_THREADS
fn get_worker_threads() -> usize {
    let configured = std::env::var(WORKER_THREADS_ENV).ok();
    let cores = std::thread::available_parallelism()
        .map(|p| p.get())
        .unwrap_or(DEFAULT_WORKER_THREADS);
    worker_threads_from(configured.as_deref(), cores)
}

/// How many worker threads to run, given what the environment said and how many
/// cores there are.
///
/// A pure function so the policy can be checked. Its test used to call
/// `get_worker_threads` and assert the answer was inside the range that
/// function's own `clamp` had just produced — true by construction, and it
/// covered none of the decisions that can actually be wrong: whether the
/// environment is honoured at all, whether an unparseable value is refused
/// rather than silently taken as zero, and whether a machine with many cores is
/// bounded.
fn worker_threads_from(configured: Option<&str>, cores: usize) -> usize {
    if let Some(value) = configured {
        match value.parse::<usize>() {
            Ok(threads) => return threads.clamp(MIN_WORKER_THREADS, MAX_WORKER_THREADS),
            Err(_) => eprintln!(
                "Warning: Invalid value for {WORKER_THREADS_ENV}: '{value}'. Using default."
            ),
        }
    }
    cores.clamp(MIN_WORKER_THREADS, MAX_WORKER_THREADS)
}

/// Get runtime configuration info
pub fn runtime_info() -> RuntimeInfo {
    RuntimeInfo {
        worker_threads: get_worker_threads(),
        env_var: WORKER_THREADS_ENV,
    }
}

/// Runtime configuration information
#[derive(Debug, Clone)]
pub struct RuntimeInfo {
    pub worker_threads: usize,
    pub env_var: &'static str,
}

/// The thread-count assertions compare `const`s, so clippy flags them as
/// constant-valued. Deliberate: they guard the relationship between the
/// defaults and the bounds, and should fail if someone edits one in isolation.
#[cfg(test)]
#[allow(clippy::assertions_on_constants)]
mod tests {
    use super::*;

    /// The environment wins when it says something usable.
    #[test]
    fn an_explicit_thread_count_is_honoured() {
        assert_eq!(worker_threads_from(Some("6"), 2), 6);
    }

    /// And is bounded either way — a request for 500 threads on a laptop is a
    /// typo, and zero would build a runtime that cannot run anything.
    #[test]
    fn an_absurd_thread_count_is_clamped_rather_than_obeyed() {
        assert_eq!(worker_threads_from(Some("500"), 8), MAX_WORKER_THREADS);
        assert_eq!(worker_threads_from(Some("0"), 8), MIN_WORKER_THREADS);
    }

    /// A value that is not a number is not a value. Parsing it as zero would
    /// produce a runtime with no workers, which hangs rather than fails.
    #[test]
    fn an_unparseable_thread_count_falls_back_to_the_core_count() {
        assert_eq!(worker_threads_from(Some("lots"), 4), 4);
        assert_eq!(worker_threads_from(Some(""), 4), 4);
    }

    /// With nothing configured, follow the machine — within bounds.
    #[test]
    fn the_core_count_is_used_when_nothing_is_configured() {
        assert_eq!(worker_threads_from(None, 4), 4);
        assert_eq!(worker_threads_from(None, 128), MAX_WORKER_THREADS);
    }

    #[test]
    fn the_default_sits_inside_the_bounds_it_is_clamped_to() {
        // Default should be at least 1
        assert!(DEFAULT_WORKER_THREADS >= MIN_WORKER_THREADS);
        // Default should be reasonable (not too high)
        assert!(DEFAULT_WORKER_THREADS <= 4);
    }
}
