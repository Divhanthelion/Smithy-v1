//! Golden PNG compare / bless.
//!
//! Goldens live under `tests/golden/` (the only path `.gitignore` keeps).
//! They are generated for human review — not blessed until a human says so.
//! Regenerating: `SMITHY_FISHERMAN_BLESS=1`.

use std::path::{Path, PathBuf};

use super::raster::PixmapInk;
use super::sheets;

/// Per-channel distance still counting as a match.
///
/// AA and platform raster differences produce ±1 noise; a quiet regenerate
/// that changes every pixel by 2 would hide a real regression. Measured
/// against a self-roundtrip of scenes.png (2026-08-03): identical. Keep 2.
pub const GOLDEN_EPSILON: u8 = 2;

/// Fraction of differing pixels allowed before fail.
///
/// Zero for a first golden set — any real drift should be reviewed. Not a
/// threshold to widen when a check goes red.
pub const GOLDEN_MAX_DIFF_FRAC: f64 = 0.0;

fn crate_tests_golden() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden")
}

fn bless_requested() -> bool {
    std::env::var("SMITHY_FISHERMAN_BLESS").is_ok_and(|v| v != "0")
}

/// Render the reviewable golden set into `tests/golden/`.
///
/// 1× only, small enough to read in a PR diff. Currently `scenes.png` —
/// the full day strip is for eyes via `target/fisherman/`, not for the
/// committed golden budget.
pub fn generate_goldens(dir: &Path) {
    std::fs::create_dir_all(dir).expect("golden dir");
    sheets::scenes_sheet(dir);
}

/// Compare current renders against committed goldens.
///
/// On mismatch writes `diff/` under `out_dir` (expected / actual / delta)
/// and returns Err. With `SMITHY_FISHERMAN_BLESS=1`, writes goldens and
/// returns Ok without comparing.
pub fn compare_or_bless(out_dir: &Path) -> Result<(), String> {
    let golden_dir = crate_tests_golden();
    if bless_requested() {
        generate_goldens(&golden_dir);
        eprintln!(
            "blessed goldens under {} — human review required before these are the spec",
            golden_dir.display()
        );
        return Ok(());
    }

    let name = "scenes.png";
    let golden_path = golden_dir.join(name);
    if !golden_path.exists() {
        // First run without goldens: generate into tests/golden for review,
        // do not fail. The PR brings them in; human blesses later.
        generate_goldens(&golden_dir);
        eprintln!(
            "no golden at {}; wrote a candidate for human review",
            golden_path.display()
        );
        return Ok(());
    }

    let tmp = out_dir.join("golden_actual");
    std::fs::create_dir_all(&tmp).ok();
    sheets::scenes_sheet(&tmp);
    let actual_path = tmp.join(name);

    let expected = load_png(&golden_path)?;
    let actual = load_png(&actual_path)?;
    if expected.width() != actual.width() || expected.height() != actual.height() {
        write_diff(out_dir, name, &expected, &actual)?;
        return Err(format!(
            "golden size mismatch for {name}: {}x{} vs {}x{}",
            expected.width(),
            expected.height(),
            actual.width(),
            actual.height()
        ));
    }

    let (diff_count, delta) = pixel_diff(&expected, &actual);
    let total = (expected.width() * expected.height()) as f64;
    let frac = diff_count as f64 / total;
    if frac > GOLDEN_MAX_DIFF_FRAC {
        let diff_dir = out_dir.join("diff");
        std::fs::create_dir_all(&diff_dir).ok();
        expected.save(&diff_dir.join(format!("{name}.expected.png")));
        actual.save(&diff_dir.join(format!("{name}.actual.png")));
        delta.save(&diff_dir.join(format!("{name}.delta.png")));
        return Err(format!(
            "golden mismatch for {name}: {diff_count} pixels differ ({:.4}%); wrote {}",
            frac * 100.0,
            diff_dir.display()
        ));
    }
    Ok(())
}

fn load_png(path: &Path) -> Result<PixmapInk, String> {
    let pm = tiny_skia::Pixmap::load_png(path)
        .map_err(|e| format!("load {}: {e}", path.display()))?;
    Ok(PixmapInk { pm })
}

fn pixel_diff(expected: &PixmapInk, actual: &PixmapInk) -> (u64, PixmapInk) {
    let w = expected.width();
    let h = expected.height();
    let mut delta = PixmapInk::new(w, h, super::raster::STEEL_DEEP);
    let mut count = 0u64;
    for y in 0..h {
        for x in 0..w {
            let e = expected.pixel(x, y).unwrap_or((0, 0, 0, 0));
            let a = actual.pixel(x, y).unwrap_or((0, 0, 0, 0));
            let dr = e.0.abs_diff(a.0);
            let dg = e.1.abs_diff(a.1);
            let db = e.2.abs_diff(a.2);
            if dr > GOLDEN_EPSILON || dg > GOLDEN_EPSILON || db > GOLDEN_EPSILON {
                count += 1;
                let i = ((y * w + x) * 4) as usize;
                let d = delta.pm.data_mut();
                d[i] = 255;
                d[i + 1] = dr.max(dg).max(db);
                d[i + 2] = 0;
                d[i + 3] = 255;
            }
        }
    }
    (count, delta)
}

fn write_diff(
    out_dir: &Path,
    name: &str,
    expected: &PixmapInk,
    actual: &PixmapInk,
) -> Result<(), String> {
    let diff_dir = out_dir.join("diff");
    std::fs::create_dir_all(&diff_dir).map_err(|e| e.to_string())?;
    expected.save(&diff_dir.join(format!("{name}.expected.png")));
    actual.save(&diff_dir.join(format!("{name}.actual.png")));
    Ok(())
}
