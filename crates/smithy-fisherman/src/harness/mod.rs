//! Automated checks and contact sheets for the fisherman.
//!
//! Order is the budget strategy: assert first (`report.json`), open a PNG
//! only when a number fails or the change was aesthetic. See
//! `docs/FISHERMAN_VERIFICATION_PLAN.md` §3–4.
//!
//! Tier A/B run via `cargo run --example sheets --features harness` and as
//! an integration test behind the same feature, so a regression cannot land
//! silently once the asserting checks are green.

pub mod font;
pub mod golden;
pub mod raster;
pub mod report;
pub mod sheets;
pub mod tier_a;
pub mod tier_b;

use std::path::Path;

use report::{Report, Summary};

/// Stage geometry the harness shares with every check and sheet.
///
/// Width 1100 / band 44 matches the live 1× rail; height `band * 3` is the
/// room the hut roof and chimney smoke need above the moulding.
pub const WIDTH: f64 = 1100.0;
pub const BAND: f64 = 44.0;
pub fn height() -> f64 {
    BAND * 3.0
}

/// Fixed sun for a deterministic simulated day.
///
/// SF-ish June-ish daylength. The absolute values do not matter as long as
/// they are fixed: the checks need the same day every run, not a real city.
pub const SUNRISE: f64 = 6.5;
pub const SUNSET: f64 = 19.5;
pub const DAY: i64 = 0;

/// Session age that leaves the hut fully built.
///
/// Teleport / moonwalk / facing on a routine day must not be polluted by the
/// build phase — that is sampled separately.
pub fn launched_built() -> f64 {
    crate::fisherman::BUILD_SECONDS
}

/// Run every Tier A + Tier B check and write `report.json`.
///
/// Returns the report so the caller can exit non-zero on asserting failures.
/// `facing_continuity` is report-only and never fails the process for flip
/// count alone.
pub fn run_checks(out_dir: &Path) -> Report {
    let mut checks = Vec::new();
    checks.extend(tier_a::run_all());
    checks.extend(tier_b::run_all());

    let mut pass = 0usize;
    let mut fail = 0usize;
    let mut report_only = 0usize;
    for c in &checks {
        if c.threshold.is_none() && c.name == "facing_continuity" {
            report_only += 1;
        } else if c.pass {
            pass += 1;
        } else {
            fail += 1;
        }
    }

    let report = Report {
        checks,
        summary: Summary {
            pass,
            fail,
            report_only,
        },
    };

    std::fs::create_dir_all(out_dir).expect("create fisherman out dir");
    let path = out_dir.join("report.json");
    let json = serde_json::to_string_pretty(&report.to_json()).expect("serialize report");
    std::fs::write(&path, json).expect("write report.json");
    eprintln!("wrote {}", path.display());
    report
}

/// Write day / midnight / scenes / build / walk contact sheets into `out_dir`.
pub fn write_sheets(out_dir: &Path) {
    std::fs::create_dir_all(out_dir).expect("create fisherman out dir");
    sheets::day_sheet(out_dir);
    sheets::midnight_sheet(out_dir);
    sheets::scenes_sheet(out_dir);
    // Eyes only — 3× so indoor tiles still have a readable silhouette in
    // the window. Not a golden; goldens stay 1×.
    sheets::scenes_sheet_3x(out_dir);
    sheets::build_sheet(out_dir);
    sheets::walk_sheet(out_dir);
}
