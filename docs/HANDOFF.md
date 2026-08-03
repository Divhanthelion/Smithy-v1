# Handoff — 2026-08-03 (evening)

Continuation after the call-graph Overview pass. For the next model
(Claude): what landed, what the user accepted by eye, what is still open.

**Manual test project:** `~/Desktop/kernelos-master`  
**Install:** `cargo install --path apps/smithy --force` → `~/.cargo/bin/smithy`  
**Prefer while iterating:** `cargo run -p smithy --release` (or
`target/release/smithy`) so ad-hoc reinstalls do not re-prompt Keychain.  
**Data dir:** `~/.local/share/smithy/`  
**Graph for kernelos:**
`~/.local/share/smithy/projects/kernelos-master-ebfea94305350e37/callgraph.json`
(~278 nodes, ~355–366 edges, 24 files)

Inspiration: [Benzi](https://github.com/shobhitx64/Benzi) / CodeMap — file-
clustered whole map + click-into neighborhood.

---

## 1. Verdict in one paragraph

§2.1 automated floor passed earlier. Call graph UI has **Overview** (whole
map, one box per source file, every symbol as a chip) and **Focus** (1-hop
neighborhood with wrap, fit camera, bus edges, jump/Back/hubs). Overview is
the default on load/build. User accepted Overview as “pretty stinkin good”
after a wider fill-the-pane pack and dropping the 8-chip/`+N` cap. Focus
fan-out rewrite from earlier is in place; double-click → editor is still
open. Keychain still prompts per item after ad-hoc reinstalls.

---

## 2. What changed this stretch (call graph)

All of this lives in `apps/smithy/src/call_graph.rs` (~2.1k lines).

### 2.1 Modes

| Mode | Behavior |
|---|---|
| **Overview** (default) | File clusters across the full center pane; chips = symbols; edges between chips; click chip → Focus |
| **Focus** | Callers above / focus / callees below; wrap; fit camera; bus edges for high fan-out |

Toolbar: Overview / Focus / Rebuild / Editor. Jump search + hub pills + Back
history work in Focus.

### 2.2 Overview layout (accepted direction)

Earlier Overview was a **tall pillar**: it reused the Focus strip width
(~720px) → ~2 columns → fit-zoom crushed it. Fixes that landed:

1. **`overview_grid_width`** — use the real pane (≈480–1800), not the Focus
   strip.
2. **Fill width** — `pick_overview_columns` takes as many columns as
   `OV_MIN_COL` (135) allows; column widths **stretch** so the grid spans
   the pane (no empty side gutters).
3. **Every symbol** — no 8-chip cap / no `+N` markers. Degree-sort hubs
   first inside each file box.
4. **Zoom LOD** — below `OV_LABEL_ZOOM` (0.55), chips draw as canvas dots;
   labels appear when zoomed in. File titles stay.
5. **Glyphs** — status / hop chrome uses ASCII (`N callers / M callees`),
   not `↑`/`↓`/middots (Menlo was tofu-boxing them).

User feedback after the first Overview pass: good, but go wider and include
the truncated symbols — both done and reinstalled.

### 2.3 Focus (earlier this session; still true)

Wrap/columnize high fan-out; `fit_camera`; opaque `BG_BASE` ground; bus
edges; moderate `default_focus` (not global max degree); two-line chips
with `file:line`; jump / hubs / Back.

### 2.4 Plumbing (earlier; still true)

- Build/load via `poll_once` (menu `Effect` was disposed → silent no-op).
- Hang fixes: do not `size.set` every paint; cache `Staleness` off the paint
  path (`CallGraphUi.stale`).
- Explicit build only; never auto-index on open.

### 2.5 Tests

```bash
cargo test -p smithy --bin smithy call_graph::
```

Includes `overview_includes_every_node_and_packs_wide` (24×20 synthetic:
every chip present, grid spans `overview_grid_width`, ≥5 columns on
~1100px).

---

## 3. What the user saw / accepted

| Moment | Verdict |
|---|---|
| First Focus strip (hub + `cmd_*` row) | **Rejected** — unreadable star/strip; hang followed (fixed) |
| Focus rewrite (wrap, fit, bus, opaque) | Landed; not re-litigated after Overview work |
| Overview v1 (file boxes, still narrow / `+N`) | “pretty stinkin good” but go wider + include everything |
| Overview v2 (wide fill + all chips) | Installed; awaiting a quick relaunch confirm |

Screenshots live in the Cursor chat assets for this thread
(`03757441-07e3-4158-869c-b7a863c271eb`).

§2.2 C–G (attachments, write-review, budgets, LSP stop/start, live
`web_search` / `explore`) were not the focus of this stretch.

---

## 4. Keychain (unchanged; still open)

macOS prompts **per Keychain item**, and ad-hoc `cargo install --force`
invalidates ACLs. App-side: provider key at connect; Brave deferred via
`WebSearch::deferred` + `secrets::is_stored`. Presence sidecar at
`~/.local/share/smithy/key_presence.json`.

**Better options (pick one next):**  
A single vault item · prefer env · real code signing · stop reinstalling
while iterating (`target/release/smithy`). Details were in the prior
handoff §4; recommendation remains **vault + don’t churn the binary**
short-term, **signing if shipping**.

---

## 5. Remaining work

### Call graph — polish / M5 closeout
- [ ] Quick relaunch confirm: Overview fills the pane, all symbols visible
      (zoom/pan as needed), no pillar / no `+N`.
- [ ] Focus hub case still readable on kernelos
      (`Terminal::execute_command` or Jump → that symbol).
- [ ] Double-click chip → open `file:line` in the editor; Editor tab
      returns to buffer.
- [ ] Optional: hover signature (schema has no signature field yet —
      qualified name + location only).

### Milestone 6 — live linking (not started)
- `touched` highlights from tool events; highlight-only; follow-agent off
  by default.

### §2.2 still open (C–G)
Attachments, write-review gate, budgets/reasoning, LSP stop/start,
`web_search` / `explore` against a live model.

### Keychain
- Implement chosen approach (A/B/C); verify **one** dialog on cold launch
  after a fresh install.

---

## 6. Landmines (still true)

- Do not name-match calls; SCIP `local N` is document-scoped; no enum
  variants in context map; reasoning never in `History`; write-review gate
  blocks; `SMITHY_LSP_LIGHT=1` OK with call graph.
- **floem:** Effects created outside a long-lived owner (menu clicks) die.
  Use `poll_once` / `exec_after` (settings pattern).
- **floem:** Never unconditional `signal.set` from a paint/canvas path.
- **`CallGraph::staleness`:** never on the UI/paint path — use
  `CallGraphUi.stale`.
- **`cargo install --force`:** re-triggers Keychain ACL prompts for ad-hoc
  binaries.
- **Overview vs Focus width:** Focus still uses `row_width_for_pane`
  (capped ~720). Overview must keep `overview_grid_width` — do not reunify
  them casually or the pillar returns.

---

## 7. File map

| Path | Role |
|---|---|
| `apps/smithy/src/call_graph.rs` | Overview + Focus UI, layout, build/load |
| `apps/smithy/src/main.rs` | Center-pane switch, menus |
| `apps/smithy/src/app_state.rs` | `CallGraphUi` on agent state |
| `apps/smithy/src/agent.rs` | Deferred Brave registration |
| `crates/smithy-agent/src/config.rs` | `secrets::{get,set,is_stored}` |
| `crates/smithy-project/src/callgraph.rs` | Library graph / staleness |
| `docs/CALLGRAPH_PLAN.md` | Architecture + milestone checklist |
| `docs/HANDOFF.md` | This file |

---

## 8. Suggested next-agent first moves

1. Relaunch smithy on kernelos, open Call Graph — confirm Overview fills
   width and includes every symbol; click into Focus on a hub and Back.
2. Wire double-click → editor at `file:line` if that is the next UX ask.
3. Otherwise: keychain vault (§4) or §2.2 write-review gate — user pick.
4. Do **not** reopen “is Overview a pillar?” unless a regression shows;
   the layout math and tests cover the failure mode.
