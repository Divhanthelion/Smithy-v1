//! Golden PNG regression for the fisherman harness.
//!
//! Goldens under `tests/golden/` are awaiting human blessing before they
//! are the specification. Regenerate with `SMITHY_FISHERMAN_BLESS=1`.

use std::path::PathBuf;

#[test]
fn scenes_golden_matches_or_is_awaiting_blessing() {
    let out = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/fisherman_golden_test");
    std::fs::create_dir_all(&out).expect("tmp");
    smithy_fisherman::harness::golden::compare_or_bless(&out).expect("golden compare");
}
