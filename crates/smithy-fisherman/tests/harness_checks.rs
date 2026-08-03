//! Tier A/B asserting checks behind the `harness` feature.
//!
//! These used to run only via the sheets example. A regression could land
//! as long as nobody re-ran the example. After the moonwalk and midnight
//! lamp fixes are green, the same `run_checks` path fails `cargo test`.

use std::path::PathBuf;

#[test]
fn tier_a_and_b_checks_pass() {
    let out = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/fisherman_checks_test");
    std::fs::create_dir_all(&out).expect("tmp");
    let report = smithy_fisherman::harness::run_checks(&out);
    assert!(
        report.all_asserting_passed(),
        "harness asserting checks failed — see {}",
        out.join("report.json").display()
    );
}
