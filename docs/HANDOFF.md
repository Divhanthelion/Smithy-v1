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

The harness found two production bugs on its first runs. Both are fixed on
`fisherman/tuning` (moonwalk + midnight lamp); Tier A/B now gate `cargo test`
behind the harness feature.

---

## 2. What landed

### 2.1 Infrastructure (`ad36a3a`, on main)

- **Checks run locally, not on GitHub.** `.githooks/pre-push` builds with
  warnings-as-errors and runs the suite before anything leaves the machine.
  One-time per clone: `git config core.hooksPath .githooks`. Bypass with
  `SKIP_CHECKS=1 git push`.

  A GitHub Actions workflow was added and then **removed**. Every run failed
  in 4–6 seconds with *"the job was not started because your account is locked
  due to a billing issue"* — an account-level lock, unrelated to this repo
  (which is public, so minutes are free). It never executed a single step, so
  the workflow was never tested, and the owner does not intend to pay GitHub.
  A permanently red check is worse than no check: it teaches you to ignore the
  colour. **Do not re-add a workflow** without first confirming Actions can
  start.

  Consequence worth carrying: every "950 green, zero warnings" in this
  document was verified by running the commands, not by any automation that
  ran on its own.
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

### 2.3 The harness — PR #2, **merged** (`c026ef0`)

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

## 3. Two production bugs — fixed on `fisherman/tuning`

### 3.1 Moonwalk at plank-trip boundaries — `does_not_moonwalk`, was 29, now 0

`face_for` clamps lookback to the current trip's start so it cannot see the
previous inbound. At `HANDOVER` it looks *into* the last outbound instead of
clamping to stillness (a second, distinct case). Guarded by unit tests and
by the harness check (now green; threshold unchanged).

### 3.2 Midnight lamp flare — fixed

`routine::sleep_progress` treats overnight sleep as one night across the
midnight split. `window_light` no longer sees progress 0 at 00:00.
Measured after: lamp stays 0.000 across 23:45 / 00:00 / 00:15.
`lighting_continuity` (within-stretch, day seed rolling) would catch a
regression; `midnight.png` is the eyes artifact.

---

## 4. This branch — `fisherman/tuning`

Done, in order:

1. §3.1 moonwalk (trip boundaries + handover).
2. §3.2 midnight lamp.
3. Day strip across midnight → `midnight.png`.
4. `lighting_continuity` check (block seams excluded; within-stretch bound
   is the door's analytical peak).
5. Tier A/B promoted to `cargo test` behind `harness`
   (`tests/harness_checks.rs`).
6. First tune pass: build sheet samples mid-outbound trips so the carry
   reads (equal completion steps had him idle at the lumber every row).
   Pose/timing feel still open — numbers cannot answer those.

```bash
cargo test --workspace                                          # 950, ~10s
cargo build --workspace --all-targets                           # 0 warnings
cargo run -p smithy-fisherman --example sheets --features harness
cargo run -p smithy --release
```

`report.json` before any PNG. Currently 11 pass / 0 fail / 1 report-only.

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
cargo test --workspace                                          # 950, ~10s
cargo build --workspace --all-targets                           # 0 warnings
cargo run -p smithy-fisherman --example sheets --features harness
cargo run -p smithy --release
```
