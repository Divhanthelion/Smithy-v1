//! Fisherman verification harness.
//!
//!     cargo run -p smithy-fisherman --example sheets --features harness
//!
//! Writes `target/fisherman/`:
//!   report.json   — every check, measured / threshold / pass
//!   day.png       — 96 cropped tiles in a grid, every 15 min
//!   midnight.png  — ~21:00–03:00 with day seed rolling (the day seam)
//!   scenes.png    — all 12 Doing states in place (1×, golden candidate)
//!   scenes_3x.png — same at 3×, eyes only (not a golden)
//!   build.png     — hut across BUILD_SECONDS
//!   walk.png      — one stride frame by frame
//!   diff/*.png    — only on golden mismatch
//!
//! Read report.json first. Open a PNG only when a check fails or the change
//! was aesthetic — the ordering is the budget strategy, not a preference. A
//! reader with eyes who opens four sheets every iteration spends the whole
//! budget on images that say "still fine".

use std::path::PathBuf;
use std::process::ExitCode;

use smithy_fisherman::harness;

fn main() -> ExitCode {
    let out = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/fisherman");
    // Prefer workspace target/ when run from the crate; fall back to cwd.
    let out = if out.exists() || out.parent().is_some_and(|p| p.exists()) {
        // Normalise to workspace `target/fisherman`.
        std::env::current_dir()
            .ok()
            .and_then(|cwd| {
                // Walk up looking for Cargo.lock / target.
                let mut cur = cwd;
                for _ in 0..6 {
                    if cur.join("Cargo.lock").exists() || cur.join("target").exists() {
                        return Some(cur.join("target/fisherman"));
                    }
                    if !cur.pop() {
                        break;
                    }
                }
                None
            })
            .unwrap_or(out)
    } else {
        PathBuf::from("target/fisherman")
    };

    std::fs::create_dir_all(&out).expect("target/fisherman");

    // 1. Checks first — the budget strategy.
    let report = harness::run_checks(&out);
    eprintln!(
        "report: pass={} fail={} report_only={}",
        report.summary.pass, report.summary.fail, report.summary.report_only
    );
    for c in &report.checks {
        let mark = if c.name == "facing_continuity" {
            ".."
        } else if c.pass {
            "ok"
        } else {
            "FAIL"
        };
        eprintln!(
            "  [{mark}] {}  measured={:.4} threshold={:?}  {}",
            c.name, c.measured, c.threshold, c.detail
        );
        if c.name == "facing_continuity" {
            eprintln!("       facing flips: {}", c.flips.len());
        }
    }

    // 2. Sheets.
    harness::write_sheets(&out);

    // 3. Goldens: generate candidates under tests/golden/ on first run /
    //    BLESS; compare when present.
    if let Err(e) = harness::golden::compare_or_bless(&out) {
        eprintln!("golden: {e}");
        return ExitCode::FAILURE;
    }

    if report.all_asserting_passed() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}
