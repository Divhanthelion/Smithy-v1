//! Repeating clocks for the decorative layers.
//!
//! Each returns a signal that counts up, and a `canvas` reading it repaints
//! when it changes. That is the whole mechanism — the same signal tracking the
//! terminal uses, rather than a render loop.
//!
//! The rates are deliberately slow and deliberately separate. The terminal must
//! never repaint at an ornament's rate: keeping those apart is most of why the
//! editor is usable, and it should not be given back for decoration.

use std::time::Duration;

use floem::reactive::{RwSignal, SignalUpdate};

/// A signal that counts up every `interval`.
///
/// Re-arms itself rather than running a thread, so it lives and dies with
/// floem's own event loop and there is nothing to shut down.
pub fn every(interval: Duration) -> RwSignal<u64> {
    let tick = RwSignal::new(0u64);
    rearm(tick, interval);
    tick
}

fn rearm(tick: RwSignal<u64>, interval: Duration) {
    floem::action::exec_after(interval, move |_| {
        tick.update(|count| *count += 1);
        rearm(tick, interval);
    });
}

/// One tick a minute — for the sky, which moves a quarter of a degree in that
/// time and is already finer than the backdrop can show.
pub fn minute() -> RwSignal<u64> {
    every(Duration::from_secs(60))
}

/// Three a second — for the stars' shimmer. Slow enough to read as
/// scintillation rather than as a strobe, and it repaints only a canvas.
pub fn shimmer() -> RwSignal<u64> {
    every(Duration::from_millis(340))
}

/// Five a second — for the fisherman. Enough for a saunter and a swaying line,
/// and nothing here ever snaps, so more would only cost repaints.
pub fn animation() -> RwSignal<u64> {
    every(Duration::from_millis(200))
}
