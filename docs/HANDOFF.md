# Handoff — 2026-08-03 (late)

Session state: what landed, what is open, what to pick up. Stable orientation
(crate map, conventions, landmines) lives in `AGENTS.md` — read that first.

**Manual test project:** `~/Desktop/kernelos-master`
**Prefer while iterating:** `cargo run -p smithy --release` — do *not*
`cargo install --force` in a loop (Keychain ACLs).
**Data dir:** `~/.local/share/smithy/`

---

## 1. One paragraph

Three tracks moved. **Repo infrastructure** now exists: CI, an agent brief, and
a `.gitignore` that no longer eats golden images. **The fisherman** got a real
verification harness — the preview used to render a *copy* of the drawing code
and now renders the drawing code itself, with automated checks, contact sheets
and a blessed golden. **The context audit** was written and nothing in it has
been implemented; that is the largest untouched item in the tree.

The harness found two production bugs on its first runs. Neither is fixed.

---

## 2. What landed

### 2.1 Infrastructure (`ad36a3a`, on main)

- `.github/workflows/ci.yml` — macOS runner (the workspace does not build on
  Linux: `smithy-voice` uses `candle-core` with the `accelerate` feature).
  Build gated at zero warnings; **clippy reports but does not gate**, with its
  ten findings enumerated and instructions to flip the ratchet.
- `AGENTS.md` — the entry point that did not exist. Crate map, build commands,
  house conventions, landmines.
- `.gitignore` — blanket `*.png` was swallowing golden images. Now
  `!**/tests/golden/**/*.png`.

### 2.2 The seam — PR #1, **merged** (`d3430ed`)

`fisherman_preview.rs` shared the *math* with `fisherman.rs` and duplicated the
*drawing*: 16 palette constants retyped, its own `draw_figure`/`draw_fish`, and
a 315-line `draw_scene` replicating hut, window, smoke, fire and props. Tuning
the flame changed no pixel in the PNG, silently.

Closed with:

- **`Ink`** — a trait with `fill` / `stroke` (checked: that is the entire paint
  surface the fisherman uses) and a defaulted `begin` (§2.3).
- **`Scene` / `scene_at` / `paint`** — the clock half separated from the drawing
  half. `paint` is pure: same `Scene`, same ink calls, forever.
- **`crates/smithy-fisherman`** — UI-free, `kurbo`/`peniko` pinned to floem's
  resolved versions. The editor keeps `fisherman_view`, the `Aesthetic` gate and
  `impl Ink for FloemInk`.
- `longitude_from_timezone` deleted — a rejected approach, not unfinished work
  (`todays_sun` documents that the timezone meridian left the frame sun and the
  backdrop sky an hour apart).

Measured drift once the preview rendered the real code: the replica had been
drawing him **facing right always** (it hardcoded `came_from = Garden`), and
**standing on the rail during Reading and Sleeping** when he should have been
inside the hut.

### 2.3 The harness — PR #2, **OPEN, ready to merge** (`9d928cc`)

`cargo run -p smithy-fisherman --example sheets --features harness` →
`target/fisherman/`.

- `report.json` — every check with measurement, threshold, pass/fail.
  **Read this before opening any PNG.** Currently 9 pass / 1 fail / 1 report-only.
- `day.png` — 96 tiles, one simulated day at 15-minute steps, cropped to
  content and laid out as a 12×8 grid (~992×540, legible in one look).
- `scenes.png` — all twelve `Doing` states in place, with props and lighting.
  **The blessed golden.** `scenes_3x.png` is the eyes-only companion.
- `build.png`, `walk.png`.
- Labels are baked into the image via a 5×7 bitmap font — no dependency.

**`Part` tagging.** Colour cannot separate the figure from the hut: `IRON`
(17,20,27) lies almost exactly on the line between `HUT_ROOF` (15,17,23) and
`HUT_WALL` (23,26,34), and their midpoint is distance 2 from `IRON`. Every
roof/wall AA edge read as figure ink — which contaminated `hidden_indoors` and
framed every indoor day-tile on a stray pixel. Fixed by tagging the draw
instead: `Ink::begin(Part)`, defaulted to a no-op so `FloemInk` and the live
paint path are untouched. The harness keeps a per-pixel mask keyed on the tag.

**Mask stamps must not antialias.** The tag is a channel value (Hut=1 …
Smoke=6), so coverage blending interpolates *between tags* — Hut under Smoke at
cov≈0.2 writes 2, which is `Figure`. `anti_alias: false` on the mask; the colour
path keeps AA.

---

## 3. Two production bugs, found by the harness, **not fixed**

Both are in shipping code. Both belong to the next branch.

### 3.1 Moonwalk at plank-trip boundaries — `does_not_moonwalk`, red, 29 frames

`face_for` subtracts 0.004 completion and can see the *previous* trip, so at
each trip boundary he walks one way while facing the other. Pre-existing:
`build_position((completion - 0.004).max(0.0))` is in `ad36a3a`, before any of
this work. Completions: `0.1115, 0.1125, 0.2225, 0.2235, … 0.8895, 0.8905,
0.9000`. Note the last one is `HANDOVER`, not a trip — a second, distinct case.

**Left red deliberately. Do not widen the threshold.**

### 3.2 Midnight lamp flare — no check catches it yet

Sleep spans midnight, but `routine::at` works in hours-since-local-midnight, so
at 00:00 the block rebuilds with `start = 0.0` and progress resets `0.900 → 0`.
`window_light` reads that as "just went to bed" and lights the lamp:

```
23.75  Sleeping  start 21.500  prog 0.900  lamp 0.000
 0.00  Sleeping  start  0.000  prog 0.000  lamp 0.450   <-- flare
 0.25  Sleeping  start  0.000  prog 0.042  lamp 0.000
```

Found by eye on `day.png`, then confirmed by probe. The harness could not have
caught it: **the day sweep never crosses midnight** — it renders 96 tiles of
`DAY` 0, so the seam between one day and the next is the one seam it never
looks at.

---

## 4. Next branch — `fisherman/tuning`

In order. Everything here came out of the harness rather than out of guessing.

1. **Fix §3.1** (both cases: trip boundaries *and* the handover at 0.9000).
2. **Fix §3.2.**
3. **Day strip across midnight** — sample ~21:00 to ~03:00 with the day seed
   rolling, as its own artifact.
4. **Lighting-continuity check** — same shape as `does_not_teleport`, but for
   `window_light` and `door_open`: flag any 1-second step where either jumps
   more than a threshold. §3.2 would have been caught by a number instead of an
   eye.
5. **Promote Tier A/B into `cargo test`** behind the `harness` feature, so a
   regression cannot land silently. **Not before 1 and 2** — CI would go red on
   a scheduled fix. The note is already in `harness/mod.rs`.
6. **Then tune.** This is the first stretch where "does it look good" is the
   only open question, because everything a number can answer now has a number.

---

## 5. Untouched: the context audit

`docs/CONTEXT_AUDIT.md` was written this session and **none of it is
implemented**. Measured: ~8k tokens before the user types. Ranked by value:

1. **Cached tokens are dropped.** `providers/sse.rs` reads `prompt_tokens`,
   `completion_tokens`, `reasoning_tokens` and nothing else. Six design
   decisions pay rent to prefix caching and nothing measures whether it works;
   the cost meter overstates cost as a result.
2. **The context ceiling lets the expensive call through.** `Budget::new` is
   inside `run_turn_inner`, so `last_prompt_tokens` resets every turn and the
   ceiling can never stop a turn's *first* request. A long session pays for one
   full prefill per turn and then stops.
3. `context_warn` is a flat 32k — useless on large-window models.
4. **The Context Usage panel** (per-segment attribution, Cursor-style) —
   designed in §4 of the audit, not built. `Session` already holds everything;
   attribute locally in chars and scale by the endpoint's reported total so the
   breakdown always sums to the billed number.
5. Per-turn tool-result budget; rank the API layer by call-graph degree.

---

## 6. Still open from before this session

- **Call graph:** double-click chip → editor at `file:line`; optional hover
  signature; Milestone 6 (live `touched` highlights).
- **Keychain:** one dialog on cold launch. Recommendation stands — single vault
  item, prefer env, stop churning the binary; sign if shipping.
- **§2.2 C–G:** attachments, write-review gate, budgets/reasoning, LSP
  stop/start, `web_search` / `explore` against a live model.
- **`DEFAULT_LOCATION` is San Francisco** "until a setting exists for it", and
  that setting still does not exist — so the fisherman's daylight is SF's for
  everyone. Deliberately out of scope all session.

---

## 7. Landmines added this session

Everything else is in `AGENTS.md`. New:

- **Tag masks must not antialias.** Coverage blending interpolates between tag
  values and invents parts that were never drawn.
- **Do not classify fisherman parts by colour.** `IRON` sits on the
  `HUT_ROOF`→`HUT_WALL` line; no radius separates them. Use `Ink::begin`.
- **`Ink::begin` is defaulted on purpose.** Keep it that way — the live paint
  path must stay free of harness concerns.
- **Goldens live under `tests/golden/`** and nowhere else; `.gitignore`
  un-ignores exactly that path and blanket-ignores every other PNG.
- **`does_not_teleport`'s bound is analytical**, not fitted:
  `1.5 × (1 / (ARRIVAL × WALK_SECONDS))`, where 1.5 is smoothstep's maximum
  derivative. It runs at ~99% utilisation *by construction*. If it fires,
  the easing changed — fix the easing or re-derive the bound, do not raise it.

---

## 8. Commands

```bash
cargo test --workspace                                          # 947, ~10s
cargo build --workspace --all-targets                           # 0 warnings
cargo run -p smithy-fisherman --example sheets --features harness
cargo run -p smithy --release
```
