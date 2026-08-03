//! Check results and the JSON report the harness writes first.
//!
//! A model with vision that opens four PNGs on every iteration burns the
//! budget on images that say "still fine." Read `report.json` first.

use serde_json::{json, Value};

/// One asserting (or report-only) check.
#[derive(Debug, Clone)]
pub struct CheckResult {
    pub name: &'static str,
    pub tier: &'static str,
    pub pass: bool,
    pub measured: f64,
    /// `None` for report-only checks (facing continuity): listed, never a
    /// process failure for the count alone.
    pub threshold: Option<f64>,
    pub detail: String,
    pub flips: Vec<FacingFlip>,
}

/// A frame where facing reversed — the lookback judgement call, inspectable.
#[derive(Debug, Clone)]
pub struct FacingFlip {
    pub second: u64,
    pub hours: f64,
    pub from: f64,
    pub to: f64,
    pub doing: String,
    pub place: String,
}

#[derive(Debug, Clone)]
pub struct Summary {
    pub pass: usize,
    pub fail: usize,
    pub report_only: usize,
}

#[derive(Debug, Clone)]
pub struct Report {
    pub checks: Vec<CheckResult>,
    pub summary: Summary,
}

impl Report {
    /// True when every asserting check passed. Facing continuity never fails
    /// the process for its flip count.
    pub fn all_asserting_passed(&self) -> bool {
        self.summary.fail == 0
    }

    pub fn to_json(&self) -> Value {
        let checks: Vec<Value> = self.checks.iter().map(CheckResult::to_json).collect();
        json!({
            "checks": checks,
            "summary": {
                "pass": self.summary.pass,
                "fail": self.summary.fail,
                "report_only": self.summary.report_only,
            }
        })
    }
}

impl CheckResult {
    pub fn to_json(&self) -> Value {
        let mut obj = json!({
            "name": self.name,
            "tier": self.tier,
            "pass": self.pass,
            "measured": self.measured,
            "threshold": self.threshold,
            "detail": self.detail,
        });
        if !self.flips.is_empty() || self.name == "facing_continuity" {
            let flips: Vec<Value> = self
                .flips
                .iter()
                .map(|f| {
                    json!({
                        "second": f.second,
                        "hours": f.hours,
                        "from": f.from,
                        "to": f.to,
                        "doing": f.doing,
                        "place": f.place,
                    })
                })
                .collect();
            obj.as_object_mut()
                .expect("object")
                .insert("flips".into(), Value::Array(flips));
        }
        obj
    }
}
