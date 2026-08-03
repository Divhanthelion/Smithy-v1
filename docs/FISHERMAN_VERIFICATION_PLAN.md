# Fisherman — an automated way to check the work

A plan for the model refining the bottom-rail animation. The goal is not more
tests; it is a **loop that closes without a human in it**: render, assert, look,
change, repeat — where "look" costs a fraction of the budget because "assert"
already caught everything a number can catch.

Written against `crates/smithy-editor/src/fisherman.rs` (76 KB, ~1.5k lines) and
`crates/smithy-editor/examples/fisherman_preview.rs` (26 KB) as of 2026-08-03.

---

## 0. The thing that has to be fixed first

**`fisherman_preview.rs` does not render `fisherman.rs`. It renders a copy of
it.**

The header comment says:

> The drawing mirrors `fisherman.rs`'s own math — same constants, same geometry
> functions — so what lands in the PNG is what lands on the rail.

That is true of the **math** and false of the **drawing**. What is actually
shared: `pose_for`, `blend`, `breathe`, `place_position`, `door_openness`,
`door_glow`, `window_light`, `hut_completion`, `build_stage`, `build_position`,
`HutGeometry`, `stage_layout`. What is **duplicated**:

| Duplicated in the preview | Lines | Original |
|---|---|---|
| 16 palette constants (private in `fisherman.rs`, retyped) | 22–39 | `fisherman.rs:145–159, 1131–1139` |
| `draw_figure` | 154 | `fisherman.rs:835` |
| `draw_fish` | 162 | `fisherman.rs:1109` |
| `draw_scene` — hut, window, smoke, fire, plank, props | 293–608 | `draw_hut`/`draw_window`/`draw_chimney_smoke`/`draw_fire`/`draw_plank`/`draw_props` |

Its own doc comment admits it: *"Replicates `draw_hut`, `draw_window`, smoke,
fire and the plank walks."*

So: tune the flame in `fisherman.rs`, run the preview, and **the PNG does not
change**. Nothing errors. Nothing warns. The model looks at a picture of the
code it did not write and concludes its edit had no effect — or worse, that an
edit it never made did. Spending a week of budget on top of that seam is
spending a week checking the wrong file.

Everything below assumes this is closed first.

---

## 1. The seam: one trait, two methods

The reason this is a small job rather than a rewrite:

```
$ grep -o "cx\.[a-z_]*(" fisherman.rs | sort | uniq -c
  24 cx.fill(
  15 cx.stroke(
```

No gradients, no clips, no images, no blend modes. The entire painting surface
the fisherman touches is **fill a path** and **stroke a path**.

```rust
/// Everything the fisherman needs to be drawn, and nothing else.
///
/// Two methods because that is genuinely all he uses — checked, not assumed.
/// A wider trait would be a wider thing to keep in sync, and the whole point
/// of this seam is that there is nothing to keep in sync.
pub trait Ink {
    fn fill(&mut self, path: &BezPath, color: Color);
    fn stroke(&mut self, path: &BezPath, color: Color, width: f64);
}
```

Two implementations: one for `floem::context::PaintCx` (the rail), one for
`tiny_skia::Pixmap` (the preview). Every `draw_*` in `fisherman.rs` changes
`cx: &mut PaintCx` to `ink: &mut impl Ink`. That is a mechanical edit across
~39 call sites and it deletes ~450 lines from the preview.

### 1.1 And a `Scene`, so both sides are driven the same way

`fisherman_view` currently computes its inputs inline from `SystemTime::now()`,
`session_seconds()` (a process-static `OnceLock<Instant>`), and the routine —
about 80 lines of decision-making between "what time is it" and "draw". None of
that is reachable from a test, which is why the preview had to reimplement the
`building` / `handover` / `came_from` / `face` logic and why those two copies
can disagree about which way he is pointing.

Split it in two:

```rust
/// Everything a frame needs. No clock, no globals — a value.
pub struct Scene {
    pub width: f64, pub height: f64, pub band: f64,
    pub doing: Doing, pub place: Place, pub previous: Place,
    pub progress: f64, pub completion: f64,
    pub frame: u64, pub seconds: f64,
}

/// The clock half. Everything nondeterministic lives here and nowhere else.
pub fn scene_at(width: f64, height: f64, band: f64, now: f64, launched: f64) -> Scene;

/// The drawing half. Pure: same Scene, same pixels, forever.
pub fn paint(ink: &mut impl Ink, scene: &Scene);
```

`fisherman_view` becomes `paint(cx, &scene_at(w, h, band, now(), launch()))`.
The preview constructs `Scene`s directly. Two consequences, both load-bearing:

1. The preview renders the **real** drawing code. The whole class of "the PNG
   lied" is gone by construction, not by discipline.
2. `scene_at` is a pure function of two timestamps, so *the day itself* becomes
   testable — including the build handover, which is currently guarded only at
   the two seams somebody thought to write a test for.

**Do not skip the `Scene` split and only do the `Ink` trait.** The trait alone
still leaves `fisherman_view`'s 80 lines of state logic unreachable, and that is
where the position/facing bugs live.

---

## 2. Move it to its own crate

`smithy-editor` pulls in eleven tree-sitter grammars, LSP, a terminal emulator,
`reqwest` and floem. An example in that crate links all of it. That is the
difference between a ten-second iteration and a two-minute one, multiplied by a
week of budget.

Propose `crates/smithy-fisherman`:

```
smithy-fisherman/
  src/routine.rs     moved from smithy-editor  (already floem-free)
  src/fisherman.rs   moved, minus fisherman_view
  src/ink.rs         the trait
  examples/sheets.rs the harness (§3)
```

Dependencies: `kurbo = "0.13.1"`, `peniko = "0.6.1"` — **pinned to exactly what
floem resolves to today** (verified: `Cargo.lock` has one version of each). Dev
dependency `tiny-skia`. Nothing else.

`smithy-editor` keeps `fisherman_view` (the floem glue) and the
`impl Ink for PaintCx`. If the versions ever diverge from floem's, the build
breaks loudly at the `impl` — which is the right place for it to break.

This is optional in the sense that everything works without it, and not optional
in the sense that it roughly triples how many iterations the budget buys.

---

## 3. The harness: assertions first, eyes second

One command, `cargo run -p smithy-fisherman --example sheets`, writes to
`target/fisherman/`:

| Artifact | What it is |
|---|---|
| `report.json` | every check, its measured value, its threshold, pass/fail |
| `day.png` | a simulated day sampled every 15 min — 96 tiles, labelled |
| `scenes.png` | all 12 `Doing` states **in place, with props and light** |
| `build.png` | the hut going up, sampled across `BUILD_SECONDS` |
| `walk.png` | one stride, frame by frame |
| `diff/*.png` | only on golden mismatch: expected, actual, difference |

**The ordering is the budget strategy.** `report.json` is a few hundred bytes of
text. Read it first. Open a PNG only when a check fails, or when the change was
aesthetic and no number can judge it. A model with vision that looks at four
1100×400 PNGs on every iteration will burn a week's budget on images that say
"still fine."

### 3.1 What the current sheets miss

`poses_sheet` covers all twelve activities — good, keep it. `scene_sheet` covers
**seven**: three build stages, Coffee, Gardening, Fishing, Cooking, Walking,
Reading, Sleeping. Never rendered in context, with their props and their
lighting: **Waking, Exercising, Eating, Siesta, Smoking**.

Those five are exactly where the bugs will be, because a pose that reads fine in
isolation is a different question from a pose next to a fire, at the doorstep,
at dusk, at true size. `scenes.png` must cover all twelve.

### 3.2 Labels have to be *in* the image

Right now the labels go to stdout (`println!("tile {i}: {label}")`) and the PNG
is unlabelled. A vision model then has to count tiles to correlate a list with a
picture, and it will miscount.

`tiny-skia` has no text. Two options:

- **Recommended:** a 5×7 uppercase-and-digits bitmap font as a `const` table.
  ~60 lines, no dependency, deterministic, and it only ever has to render
  `SMOKING 14:15`. It matches how the rest of this tree treats small problems.
- `ab_glyph` as a dev-dependency, if legibility at 1× turns out to matter more
  than the sixty lines.

Either way, each tile carries its own label and the sheet is self-describing.

---

## 4. The checks

Each one exists because of a failure that has already happened here or is one
edit away. Each carries the failure in a comment — house style, and it is what
stops the next model from "fixing" a red check by widening the threshold.

### Tier A — geometry and containment

| Check | Assertion | The bug it catches |
|---|---|---|
| **He stays on the rail** | no ink outside `[h - band, h]`, or left of `band * 2.1` | "the hut grows out of the corner stone" — the clearance `stage_layout` documents, currently enforced only by a comment. Also the rod poking into the editor pane. |
| **He exists** | outdoors → figure bbox non-empty and inside the stage | he "snapped straight out of existence" at the wall. Tested at the door; untested for the other four places. |
| **He is hidden when indoors** | `is_indoors` → zero figure ink, and the window is lit | the inverse: drawn *and* in the window, i.e. twice. |
| **He is the right size** | figure bbox height ∈ `[0.5, 1.1] × scale` | a scale bug that makes him a smudge or a giant. Cheap; catches an entire class. |
| **He does not teleport** | sample the whole day at 1 s steps: max Δposition per step < one stride | every discontinuity in the routine, the build, and the handover — at once. This is the single highest-value check here, and it is what `HANDOVER` and `ARRIVAL` exist to satisfy. |
| **He does not moonwalk** | whenever Δposition < 0, `facing` < 0 | the loaded-trip bug, generalised past the two cases with tests. |

### Tier B — the picture

| Check | Assertion | The bug it catches |
|---|---|---|
| **Contrast** | mean luminance of `RIM` stroke pixels vs the `STEEL_*` behind them > threshold | he vanishes against the frame. Currently only judged by eye, and only in the light the tester happened to render. |
| **Ink budget** | non-background coverage per tile ∈ `[lo, hi]` | a blank frame, and a frame that is one solid blob. |
| **The fire is where the fire is** | `FIRE_*` pixels within the pit's bbox | "a hearth that teleports to the doorstep reads as a decal" — the comment at `fisherman.rs:825` is the check. |
| **Light agrees with itself** | `door_glow > 0` ⇒ lit pixels near the doorway; lamp out ⇒ none | "a shut door never glows and a dark room never spills." Same: the comment is already the assertion. |

### Tier C — regression

Golden PNGs under `crates/smithy-fisherman/tests/golden/`, 1× only (they stay
small and reviewable in a diff). Compare per-pixel with a small epsilon; on
mismatch write `diff/` and fail.

Goldens are **committed and reviewed like code**. Regenerating them is
`SMITHY_FISHERMAN_BLESS=1`, and blessing a golden is a decision that shows up in
the diff — which is the point. A golden quietly regenerated is a test deleted.

### The rule that keeps this honest

> A threshold may be changed only in a commit that says which real frame
> motivated it, and shows the frame. Widening a threshold to make a check pass
> is deleting the check.

---

## 5. What this does not check

Say it out loud so nobody trusts the harness past where it earns trust:

- **Whether it looks good.** The checks catch broken, not ugly. "Does the
  seated pose read as settling in rather than crouching to spring" is a
  judgement, and it stays a judgement. That is what the eyes and the sheets are
  for.
- **Timing feel.** `the_secondary_motions_never_settle_into_one_visible_loop`
  already guards the one timing property that is mathematically checkable.
  Whether the walk *reads* at 12 seconds is not.
- **The live floem path.** The `Ink` seam guarantees the same drawing code runs,
  not that floem's rasteriser and tiny-skia agree pixel-for-pixel. They will not,
  exactly. This is fine: the goldens are tiny-skia's, and a floem-side rendering
  bug is a floem bug, not a fisherman bug. Do one manual launch per session to
  confirm the rail still looks like the sheet.

---

## 6. Order of work

1. `Ink` trait + `impl` for `PaintCx`; convert `fisherman.rs`'s `draw_*`.
   Existing tests must still pass untouched — they test math, and math does not
   move.
2. `Scene` + `scene_at` + `paint`; `fisherman_view` shrinks to three lines.
3. Rewrite `fisherman_preview.rs` to call `paint`. **Delete every duplicated
   constant and `draw_*`.** The diff should be about −450 lines. If it is not,
   something is still duplicated.
4. Move to `crates/smithy-fisherman`. (Steps 1–3 first — moving code that is
   still forked doubles the work.)
5. Tier A checks + `report.json`.
6. `day.png` / `scenes.png` / labels.
7. Tier B checks.
8. Bless the first goldens. **Only after a human has looked at them once** — a
   golden blessed from a broken frame makes broken the specification.
9. Then, and only then, start tuning.

Steps 1–4 are the week's leverage. Steps 5–8 are what let the rest of the week
be spent on the animation instead of on wondering.

---

## 7. Landmines

- **`session_seconds()` is a process-static `OnceLock<Instant>`.** It cannot be
  reset, so two scenes in one process share a launch time. This is why
  `scene_at` must take `launched` as a parameter rather than reading it.
- **Existing tests are all math tests and all good.** Nineteen in
  `fisherman.rs`, six in `routine.rs`, each documenting the frame that motivated
  it. Do not replace them with pixel checks — pixel checks are the layer *above*
  them, not a substitute. Keep both.
- **Two assertions compare `const`s** and are `#[allow(clippy::assertions_on_constants)]`
  deliberately. Leave the allow.
- **The palette is private in `fisherman.rs`.** After the seam, make it `pub` (or
  a `pub struct Palette`) and delete the preview's copy. Leaving both is the
  original bug in a smaller box.
- **`Aesthetic::Forged` gate and the `w < 240.0 || h < band * 3.0` early return**
  live in `fisherman_view`. They belong there, not in `paint` — `paint` should
  draw whatever `Scene` it is handed, so the harness can render sizes the app
  refuses.
- **`floem`: never unconditional `signal.set` from a paint path** (HANDOFF §6).
  Nothing here should add one; `paint` takes a value and returns nothing.
